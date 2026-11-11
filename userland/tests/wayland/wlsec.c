/*
 * wlsec : banc de conformité de MaiLand (serveur Wayland du noyau de MaiOS), phase W4 :
 * security-context-v1 (bacs à sable façon Flatpak) et sélection primaire, sans fenêtre ni
 * intervention du harnais.
 *
 *   - les globales wp_security_context_manager_v1 et zwp_primary_selection_device_manager_v1
 *     sont annoncées ;
 *   - un contexte (socket d'écoute + tube de fin) est validé ; une connexion à cette socket est
 *     acceptée par le serveur et ne voit plus le gestionnaire (globale privilégiée), le lier
 *     est une erreur ;
 *   - sélection primaire depuis cette connexion : sans focus, la source reçoit cancelled ;
 *   - erreurs : listen_fd qui n'écoute pas (invalid_listen_fd), métadonnée posée deux fois
 *     (already_set), requête après commit (already_used) ;
 *   - fin du bac à sable (tube de fin fermé) : le serveur n'accepte plus de connexion.
 *
 * Sortie : lignes [ok] / [ECHEC], puis « wlsec : FIN <ok> ok, <echec> echec ».
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <wayland-client.h>
#include "primary-selection-unstable-v1-client-protocol.h"
#include "security-context-v1-client-protocol.h"

static int n_ok, n_ko;

#define CHECK(cond, ...)                         \
    do {                                         \
        if (cond) {                              \
            n_ok++;                              \
            printf("[ok] ");                     \
        } else {                                 \
            n_ko++;                              \
            printf("[ECHEC] ");                  \
        }                                        \
        printf(__VA_ARGS__);                     \
        printf("\n");                            \
        fflush(stdout);                          \
    } while (0)

struct conn {
    struct wl_display *d;
    struct wl_registry *reg;
    uint32_t sec_name, prim_name, seat_name, comp_name;
    struct wp_security_context_manager_v1 *sec;
    struct zwp_primary_selection_device_manager_v1 *prim;
    struct wl_seat *seat;
    int cancelled;
};

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    struct conn *c = data;
    (void)v;
    if (!strcmp(iface, wp_security_context_manager_v1_interface.name)) {
        c->sec_name = name;
        c->sec = wl_registry_bind(r, name, &wp_security_context_manager_v1_interface, 1);
    } else if (!strcmp(iface, zwp_primary_selection_device_manager_v1_interface.name)) {
        c->prim_name = name;
        c->prim = wl_registry_bind(r, name, &zwp_primary_selection_device_manager_v1_interface, 1);
    } else if (!strcmp(iface, wl_seat_interface.name)) {
        c->seat_name = name;
        c->seat = wl_registry_bind(r, name, &wl_seat_interface, 1);
    } else if (!strcmp(iface, wl_compositor_interface.name)) {
        c->comp_name = name;
    }
}

static void reg_remove(void *data, struct wl_registry *r, uint32_t name) {
    (void)data, (void)r, (void)name;
}

static const struct wl_registry_listener reg_listener = {reg_global, reg_remove};

static int setup(struct conn *c, struct wl_display *d) {
    memset(c, 0, sizeof *c);
    c->d = d;
    if (!d) return -1;
    c->reg = wl_display_get_registry(d);
    wl_registry_add_listener(c->reg, &reg_listener, c);
    return wl_display_roundtrip(d) < 0 ? -1 : 0;
}

static void expect_error(struct conn *c, const char *what, const char *iface, uint32_t code) {
    wl_display_roundtrip(c->d);
    const struct wl_interface *i = NULL;
    uint32_t id = 0;
    int err = wl_display_get_error(c->d);
    uint32_t got = err == EPROTO ? wl_display_get_protocol_error(c->d, &i, &id) : 0;
    CHECK(err == EPROTO && i && !strcmp(i->name, iface) && got == code, "%s : erreur %s.%u (recu errno %d, %s.%u)", what, iface,
          code, err, i ? i->name : "-", got);
}

/* Socket d'écoute dans $XDG_RUNTIME_DIR. */
static int listen_socket(const char *path) {
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (s < 0) return -1;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s", path);
    unlink(path);
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, 8) < 0) {
        close(s);
        return -1;
    }
    return s;
}

static int connect_socket(const char *path) {
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (s < 0) return -1;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s", path);
    if (connect(s, (struct sockaddr *)&a, sizeof a) < 0) {
        close(s);
        return -1;
    }
    return s;
}

static void src_send(void *data, struct zwp_primary_selection_source_v1 *s, const char *mime, int32_t fd) {
    (void)data, (void)s, (void)mime;
    close(fd);
}

static void src_cancelled(void *data, struct zwp_primary_selection_source_v1 *s) {
    (void)s;
    ((struct conn *)data)->cancelled = 1;
}

static const struct zwp_primary_selection_source_v1_listener src_listener = {src_send, src_cancelled};

/* Contexte validé avec ses métadonnées ; rend 0 si le serveur n'a rien refusé. */
static struct wp_security_context_v1 *make_context(struct conn *c, int listen_fd, int close_fd) {
    struct wp_security_context_v1 *x = wp_security_context_manager_v1_create_listener(c->sec, listen_fd, close_fd);
    wp_security_context_v1_set_sandbox_engine(x, "org.maios.test");
    wp_security_context_v1_set_app_id(x, "org.maios.wlsec");
    wp_security_context_v1_set_instance_id(x, "1");
    return x;
}

int main(void) {
    printf("wlsec : debut (pid %d)\n", (int)getpid());
    fflush(stdout);
    const char *rt = getenv("XDG_RUNTIME_DIR");
    char path[200];
    snprintf(path, sizeof path, "%s/wlsec-%d", rt ? rt : "/tmp", (int)getpid());

    struct conn main_c;
    if (setup(&main_c, wl_display_connect(NULL)) < 0) {
        printf("wlsec : connexion impossible\nwlsec : FIN 0 ok, 1 echec\n");
        return 1;
    }
    CHECK(main_c.sec != NULL, "globale wp_security_context_manager_v1 annoncee");
    CHECK(main_c.prim != NULL, "globale zwp_primary_selection_device_manager_v1 annoncee");
    if (!main_c.sec) {
        printf("wlsec : FIN %d ok, %d echec\n", n_ok, n_ko);
        return 1;
    }

    /* Contexte valide. */
    int ls = listen_socket(path);
    int endp[2];
    CHECK(ls >= 0 && pipe2(endp, O_CLOEXEC) == 0, "socket d'ecoute %s et tube de fin", path);
    struct wp_security_context_v1 *ctx = make_context(&main_c, ls, endp[0]);
    wp_security_context_v1_commit(ctx);
    wp_security_context_v1_destroy(ctx);
    CHECK(wl_display_roundtrip(main_c.d) >= 0, "contexte valide (commit)");
    close(endp[0]);

    /* Connexion du bac à sable. */
    struct conn box;
    int bs = connect_socket(path);
    int r = bs >= 0 ? setup(&box, wl_display_connect_to_fd(bs)) : -1;
    CHECK(r == 0, "connexion du bac a sable acceptee par le serveur");
    if (r == 0) {
        CHECK(box.comp_name != 0, "bac a sable : wl_compositor visible");
        CHECK(box.sec == NULL, "bac a sable : wp_security_context_manager_v1 cachee");
        CHECK(box.prim != NULL, "bac a sable : selection primaire visible");
        if (box.prim && box.seat) {
            struct zwp_primary_selection_device_v1 *dev = zwp_primary_selection_device_manager_v1_get_device(box.prim, box.seat);
            struct zwp_primary_selection_source_v1 *src = zwp_primary_selection_device_manager_v1_create_source(box.prim);
            zwp_primary_selection_source_v1_add_listener(src, &src_listener, &box);
            zwp_primary_selection_source_v1_offer(src, "text/plain;charset=utf-8");
            zwp_primary_selection_device_v1_set_selection(dev, src, 0);
            wl_display_roundtrip(box.d);
            CHECK(box.cancelled, "selection primaire sans focus : source annulee");
            zwp_primary_selection_source_v1_destroy(src);
            zwp_primary_selection_device_v1_destroy(dev);
        }
        /* Lier la globale cachée (nom vu par la connexion principale) : erreur portée par le
         * registre, comme `registry_bind` de libwayland-server. */
        wl_registry_bind(box.reg, main_c.sec_name, &wp_security_context_manager_v1_interface, 1);
        expect_error(&box, "bac a sable : lier la globale privilegiee", "wl_registry", WL_DISPLAY_ERROR_INVALID_OBJECT);
        wl_display_disconnect(box.d);
    }

    /* Fin du bac à sable : le serveur n'accepte plus (aucune réponse en 1 s). */
    close(endp[1]);
    wl_display_roundtrip(main_c.d);
    int late = connect_socket(path);
    int answered = -1;
    if (late >= 0) {
        static const uint32_t sync_msg[3] = {1, (12u << 16) | 0 /* wl_display.sync */, 2};
        if (write(late, sync_msg, sizeof sync_msg) == (ssize_t)sizeof sync_msg) {
            struct pollfd p = {.fd = late, .events = POLLIN};
            answered = poll(&p, 1, 1000);
        }
        close(late);
    }
    CHECK(answered == 0, "tube de fin ferme : plus de connexion acceptee (poll %d)", answered);
    close(ls);
    unlink(path);

    /* Erreurs, chacune sur une connexion neuve. */
    struct conn e;
    if (setup(&e, wl_display_connect(NULL)) == 0 && e.sec) {
        int p[2];
        pipe2(p, O_CLOEXEC);
        wp_security_context_manager_v1_create_listener(e.sec, p[0], p[1]);
        expect_error(&e, "listen_fd qui n'ecoute pas", "wp_security_context_manager_v1",
                     WP_SECURITY_CONTEXT_MANAGER_V1_ERROR_INVALID_LISTEN_FD);
        close(p[0]), close(p[1]);
        wl_display_disconnect(e.d);
    }
    if (setup(&e, wl_display_connect(NULL)) == 0 && e.sec) {
        int s = listen_socket(path), p[2];
        pipe2(p, O_CLOEXEC);
        struct wp_security_context_v1 *x = make_context(&e, s, p[0]);
        wp_security_context_v1_set_app_id(x, "encore");
        expect_error(&e, "app_id pose deux fois", "wp_security_context_v1", WP_SECURITY_CONTEXT_V1_ERROR_ALREADY_SET);
        close(s), close(p[0]), close(p[1]);
        unlink(path);
        wl_display_disconnect(e.d);
    }
    if (setup(&e, wl_display_connect(NULL)) == 0 && e.sec) {
        int s = listen_socket(path), p[2];
        pipe2(p, O_CLOEXEC);
        struct wp_security_context_v1 *x = make_context(&e, s, p[0]);
        wp_security_context_v1_commit(x);
        wp_security_context_v1_commit(x);
        expect_error(&e, "commit apres commit", "wp_security_context_v1", WP_SECURITY_CONTEXT_V1_ERROR_ALREADY_USED);
        close(s), close(p[0]), close(p[1]);
        unlink(path);
        wl_display_disconnect(e.d);
    }

    wl_display_disconnect(main_c.d);
    printf("wlsec : FIN %d ok, %d echec\n", n_ok, n_ko);
    fflush(stdout);
    return n_ko != 0;
}
