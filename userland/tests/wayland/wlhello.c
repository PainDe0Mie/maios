/*
 * wlhello : banc de conformité de MaiLand (serveur Wayland du noyau de MaiOS), phase W1.
 *
 * Client libwayland-client de Debian (glibc dynamique), comme les vraies applications :
 *   - registre, wl_output, formats wl_shm, latence d'un aller-retour (wl_display.sync) ;
 *   - fenêtre xdg_toplevel : configuration initiale, tampon memfd, entrée sur l'écran, rappels
 *     frame (cadence), dommages partiels, changement de taille ; wl_buffer.release quand un
 *     autre tampon remplace celui que MaiCompositor lit directement (composition directe) ;
 *   - erreurs de protocole attendues (le serveur doit les signaler, pas planter) : numéro de
 *     configuration inconnu, tampon avant configuration, pas de ligne trop petit ;
 *   - 32 connexions simultanées, octets aléatoires sur une socket brute, puis le serveur doit
 *     toujours répondre.
 *
 *   wlhello [secondes]   (garde la fenêtre affichée ce temps-là, pour une capture)
 *
 * Sortie : lignes [ok] / [ECHEC], puis « wltest : FIN <ok> ok, <echec> echec ».
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "xdg-decoration-unstable-v1-client-protocol.h"

/* Décorations du serveur demandées (`xdg-decoration`) : sans objet de décoration, MaiLand
 * laisse le client décorer (mode par défaut du protocole) et les captures vérifiées au pixel
 * près supposent la barre de titre du système. */
static struct zxdg_decoration_manager_v1 *deco_mgr;
static void server_side(struct xdg_toplevel *top) {
    if (deco_mgr) {
        struct zxdg_toplevel_decoration_v1 *d = zxdg_decoration_manager_v1_get_toplevel_decoration(deco_mgr, top);
        zxdg_toplevel_decoration_v1_set_mode(d, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
    }
}

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

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

struct buf {
    struct wl_buffer *wb;
    uint32_t *px;
    int w, h, busy;
};

struct app {
    struct wl_display *d;
    struct wl_registry *reg;
    struct wl_compositor *comp;
    struct wl_shm *shm;
    struct xdg_wm_base *wm;
    struct wl_output *out;
    int has_argb, has_xrgb;
    int out_w, out_h, out_done;
    char out_name[64];
    struct wl_surface *surf;
    struct xdg_surface *xs;
    struct xdg_toplevel *top;
    uint32_t serial;
    int configured, cfg_w, cfg_h, bounds_w, bounds_h, caps, closed, entered;
    int frame_done, releases;
    /* Pool memfd. */
    int fd;
    size_t pool_size;
    uint8_t *pool;
    struct wl_shm_pool *wpool;
};

/* --- auditeurs --------------------------------------------------------------------------- */

static void shm_format(void *data, struct wl_shm *shm, uint32_t f) {
    struct app *a = data;
    (void)shm;
    if (f == WL_SHM_FORMAT_ARGB8888) a->has_argb = 1;
    if (f == WL_SHM_FORMAT_XRGB8888) a->has_xrgb = 1;
}
static const struct wl_shm_listener shm_listener = {shm_format};

static void out_geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t pw, int32_t ph, int32_t sp,
                         const char *make, const char *model, int32_t tr) {
    (void)d, (void)o, (void)x, (void)y, (void)pw, (void)ph, (void)sp, (void)make, (void)model, (void)tr;
}
static void out_mode(void *data, struct wl_output *o, uint32_t flags, int32_t w, int32_t h, int32_t r) {
    struct app *a = data;
    (void)o, (void)r;
    if (flags & WL_OUTPUT_MODE_CURRENT) a->out_w = w, a->out_h = h;
}
static void out_done(void *data, struct wl_output *o) {
    (void)o;
    ((struct app *)data)->out_done = 1;
}
static void out_scale(void *d, struct wl_output *o, int32_t s) { (void)d, (void)o, (void)s; }
static void out_name(void *data, struct wl_output *o, const char *n) {
    (void)o;
    snprintf(((struct app *)data)->out_name, 64, "%s", n);
}
static void out_desc(void *d, struct wl_output *o, const char *s) { (void)d, (void)o, (void)s; }
static const struct wl_output_listener out_listener = {out_geometry, out_mode, out_done, out_scale, out_name, out_desc};

static void wm_ping(void *d, struct xdg_wm_base *wm, uint32_t serial) {
    (void)d;
    xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = {wm_ping};

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    struct app *a = data;
#define MIN(x, y) ((x) < (y) ? (x) : (y))
    if (!strcmp(iface, wl_compositor_interface.name)) {
        a->comp = wl_registry_bind(r, name, &wl_compositor_interface, MIN(v, 6));
    } else if (!strcmp(iface, wl_shm_interface.name)) {
        a->shm = wl_registry_bind(r, name, &wl_shm_interface, MIN(v, 2));
        wl_shm_add_listener(a->shm, &shm_listener, a);
    } else if (!strcmp(iface, zxdg_decoration_manager_v1_interface.name)) {
        deco_mgr = wl_registry_bind(r, name, &zxdg_decoration_manager_v1_interface, 1);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, MIN(v, 6));
        xdg_wm_base_add_listener(a->wm, &wm_listener, a);
    } else if (!strcmp(iface, wl_output_interface.name) && !a->out) {
        a->out = wl_registry_bind(r, name, &wl_output_interface, MIN(v, 4));
        wl_output_add_listener(a->out, &out_listener, a);
    }
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d, (void)r, (void)n; }
static const struct wl_registry_listener reg_listener = {reg_global, reg_remove};

static void xs_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
    struct app *a = data;
    (void)xs;
    a->serial = serial;
    a->configured++;
}
static const struct xdg_surface_listener xs_listener = {xs_configure};

static void top_configure(void *data, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *states) {
    struct app *a = data;
    (void)t, (void)states;
    a->cfg_w = w, a->cfg_h = h;
}
static void top_close(void *data, struct xdg_toplevel *t) {
    (void)t;
    ((struct app *)data)->closed = 1;
}
static void top_bounds(void *data, struct xdg_toplevel *t, int32_t w, int32_t h) {
    struct app *a = data;
    (void)t;
    a->bounds_w = w, a->bounds_h = h;
}
static void top_caps(void *data, struct xdg_toplevel *t, struct wl_array *caps) {
    struct app *a = data;
    uint32_t *c;
    (void)t;
    wl_array_for_each(c, caps) a->caps |= 1 << *c;
}
static const struct xdg_toplevel_listener top_listener = {top_configure, top_close, top_bounds, top_caps};

static void surf_enter(void *data, struct wl_surface *s, struct wl_output *o) {
    (void)s, (void)o;
    ((struct app *)data)->entered = 1;
}
static void surf_leave(void *d, struct wl_surface *s, struct wl_output *o) { (void)d, (void)s, (void)o; }
static void surf_scale(void *d, struct wl_surface *s, int32_t f) { (void)d, (void)s, (void)f; }
static void surf_transform(void *d, struct wl_surface *s, uint32_t t) { (void)d, (void)s, (void)t; }
static const struct wl_surface_listener surf_listener = {surf_enter, surf_leave, surf_scale, surf_transform};

static void buf_release(void *data, struct wl_buffer *wb) {
    struct buf *b = data;
    (void)wb;
    b->busy = 0;
}
static const struct wl_buffer_listener buf_listener = {buf_release};

static void frame_done(void *data, struct wl_callback *cb, uint32_t t) {
    (void)t;
    ((struct app *)data)->frame_done = 1;
    wl_callback_destroy(cb);
}
static const struct wl_callback_listener frame_listener = {frame_done};

/* --- boucle ------------------------------------------------------------------------------ */

/* Distribue les événements jusqu'à `*flag` (ou le délai) ; -1 : délai ou erreur. */
static int pump(struct wl_display *d, volatile int *flag, int ms) {
    double end = now_ms() + ms;
    while (!*flag) {
        while (wl_display_prepare_read(d) != 0) wl_display_dispatch_pending(d);
        if (*flag) {
            wl_display_cancel_read(d);
            break;
        }
        wl_display_flush(d);
        int left = (int)(end - now_ms());
        struct pollfd p = {wl_display_get_fd(d), POLLIN, 0};
        if (left <= 0 || poll(&p, 1, left) <= 0) {
            wl_display_cancel_read(d);
            return -1;
        }
        if (wl_display_read_events(d) < 0) return -1;
        if (wl_display_dispatch_pending(d) < 0) return -1;
    }
    return 0;
}

static int connect_app(struct app *a) {
    memset(a, 0, sizeof *a);
    a->fd = -1;
    a->d = wl_display_connect(NULL);
    if (!a->d) return -1;
    a->reg = wl_display_get_registry(a->d);
    wl_registry_add_listener(a->reg, &reg_listener, a);
    if (wl_display_roundtrip(a->d) < 0 || wl_display_roundtrip(a->d) < 0) return -1;
    return a->comp && a->shm && a->wm ? 0 : -1;
}

static int make_pool(struct app *a, size_t size) {
    a->fd = memfd_create("wlhello", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (a->fd < 0 || ftruncate(a->fd, (off_t)size) < 0) return -1;
    a->pool = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, a->fd, 0);
    if (a->pool == MAP_FAILED) return -1;
    a->pool_size = size;
    a->wpool = wl_shm_create_pool(a->shm, a->fd, (int32_t)size);
    return 0;
}

static void make_buf(struct app *a, struct buf *b, size_t offset, int w, int h) {
    b->w = w, b->h = h, b->busy = 0;
    b->px = (uint32_t *)(a->pool + offset);
    b->wb = wl_shm_pool_create_buffer(a->wpool, (int32_t)offset, w, h, w * 4, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(b->wb, &buf_listener, b);
}

/* Dégradé avec une barre verticale à la position `bar`. */
static void draw(struct buf *b, int bar) {
    for (int y = 0; y < b->h; y++) {
        uint32_t *row = b->px + (size_t)y * b->w;
        for (int x = 0; x < b->w; x++) {
            uint32_t r = (uint32_t)(x * 255 / b->w), g = (uint32_t)(y * 255 / b->h);
            row[x] = (x >= bar && x < bar + 24) ? 0xffffd23f : 0xff000000 | r << 16 | g << 8 | 0x60;
        }
    }
}

static void teardown(struct app *a) {
    if (a->d) wl_display_disconnect(a->d);
    if (a->pool && a->pool != MAP_FAILED) munmap(a->pool, a->pool_size);
    if (a->fd >= 0) close(a->fd);
    memset(a, 0, sizeof *a);
}

/* Fenêtre configurée (commit initial + attente de la configuration). */
static int open_window(struct app *a, const char *title) {
    a->surf = wl_compositor_create_surface(a->comp);
    wl_surface_add_listener(a->surf, &surf_listener, a);
    a->xs = xdg_wm_base_get_xdg_surface(a->wm, a->surf);
    xdg_surface_add_listener(a->xs, &xs_listener, a);
    a->top = xdg_surface_get_toplevel(a->xs);
    xdg_toplevel_add_listener(a->top, &top_listener, a);
    xdg_toplevel_set_title(a->top, title);
    xdg_toplevel_set_app_id(a->top, "org.maios.wlhello");
    server_side(a->top);
    wl_surface_commit(a->surf);
    return pump(a->d, &a->configured, 3000);
}

/* Erreur de protocole attendue : interface et code. */
static void expect_error(struct app *a, const char *what, const char *iface, uint32_t code) {
    wl_display_roundtrip(a->d);
    const struct wl_interface *i = NULL;
    uint32_t id = 0;
    int err = wl_display_get_error(a->d);
    uint32_t c = err == EPROTO ? wl_display_get_protocol_error(a->d, &i, &id) : 0;
    CHECK(err == EPROTO && i && !strcmp(i->name, iface) && c == code, "%s : erreur %s.%u (recu errno %d, %s.%u)", what, iface,
          code, err, i ? i->name : "-", c);
}

int main(int argc, char **argv) {
    int hold = argc > 1 ? atoi(argv[1]) : 0;
    struct app a;
    printf("wltest : debut (pid %d)\n", getpid());
    fflush(stdout);

    /* 1. Connexion et globales. */
    int r = connect_app(&a);
    CHECK(r == 0, "connexion a %s/%s et globales (compositor, shm, xdg_wm_base)", getenv("XDG_RUNTIME_DIR"),
          getenv("WAYLAND_DISPLAY"));
    if (r) {
        printf("wltest : FIN %d ok, %d echec\n", n_ok, n_ko + 1);
        return 1;
    }
    CHECK(a.out && a.out_done && a.out_w > 0 && a.out_h > 0, "wl_output %s : %dx%d", a.out_name, a.out_w, a.out_h);
    CHECK(a.has_argb && a.has_xrgb, "formats wl_shm ARGB8888 et XRGB8888");

    /* 2. Latence d'un aller-retour. */
    double t0 = now_ms();
    int n = 500;
    for (int i = 0; i < n; i++) wl_display_roundtrip(a.d);
    double rt = (now_ms() - t0) * 1000.0 / n;
    printf("wltest : aller-retour %.1f us\n", rt);
    CHECK(rt < 2000.0, "aller-retour wl_display.sync en %.1f us", rt);

    /* 3. Fenêtre. */
    r = open_window(&a, "wlhello - MaiLand");
    CHECK(r == 0 && a.configured && a.cfg_w == 0 && a.cfg_h == 0, "configuration initiale (serie %u, %dx%d)", a.serial,
          a.cfg_w, a.cfg_h);
    uint32_t tv = xdg_toplevel_get_version(a.top);
    if (tv >= XDG_TOPLEVEL_CONFIGURE_BOUNDS_SINCE_VERSION)
        CHECK(a.bounds_w > 0 && a.bounds_h > 0, "configure_bounds %dx%d", a.bounds_w, a.bounds_h);
    if (tv >= XDG_TOPLEVEL_WM_CAPABILITIES_SINCE_VERSION)
        CHECK(a.caps & (1 << XDG_TOPLEVEL_WM_CAPABILITIES_MINIMIZE), "wm_capabilities (reduire)");
    xdg_surface_ack_configure(a.xs, a.serial);

    const int W = 480, H = 320, W2 = 640, H2 = 400;
    struct buf b[3];
    r = make_pool(&a, (size_t)W * H * 4 * 2 + (size_t)W2 * H2 * 4);
    CHECK(r == 0, "pool memfd de %zu octets", a.pool_size);
    make_buf(&a, &b[0], 0, W, H);
    make_buf(&a, &b[1], (size_t)W * H * 4, W, H);
    make_buf(&a, &b[2], (size_t)W * H * 8, W2, H2);

    draw(&b[0], 0);
    wl_surface_attach(a.surf, b[0].wb, 0, 0);
    wl_surface_damage_buffer(a.surf, 0, 0, W, H);
    a.frame_done = 0;
    wl_callback_add_listener(wl_surface_frame(a.surf), &frame_listener, &a);
    b[0].busy = 1;
    wl_surface_commit(a.surf);
    r = pump(a.d, &a.frame_done, 3000);
    wl_display_roundtrip(a.d);
    CHECK(r == 0, "premiere image : rappel frame");
    CHECK(b[0].busy, "premiere image : tampon garde (lu directement par le compositeur)");
    CHECK(a.entered, "wl_surface.enter sur l'ecran");

    /* 4. Animation : 120 images au rythme des rappels frame, dommages partiels. */
    int frames = 120, cur = 1, prev_bar = 0, lost = 0;
    t0 = now_ms();
    for (int i = 1; i <= frames; i++) {
        struct buf *bb = &b[cur];
        /* MaiLand rend un tampon dès qu'un autre le remplace : libre une image plus tard. */
        if (bb->busy) lost++;
        int bar = (i * 7) % (W - 24);
        draw(bb, bar);
        wl_surface_attach(a.surf, bb->wb, 0, 0);
        /* Le tampon qui revient a deux images de retard : on abîme les deux positions. */
        wl_surface_damage_buffer(a.surf, prev_bar, 0, 24 + 14, H);
        wl_surface_damage_buffer(a.surf, bar, 0, 24, H);
        prev_bar = bar > 14 ? bar - 14 : 0;
        a.frame_done = 0;
        wl_callback_add_listener(wl_surface_frame(a.surf), &frame_listener, &a);
        bb->busy = 1;
        wl_surface_commit(a.surf);
        if (pump(a.d, &a.frame_done, 2000) < 0) {
            lost++;
            break;
        }
        cur ^= 1;
    }
    double dt = now_ms() - t0;
    double fps = frames * 1000.0 / dt;
    printf("wltest : %d images en %.0f ms (%.1f i/s)\n", frames, dt, fps);
    CHECK(lost == 0 && fps > 30.0 && fps < 75.0, "cadence des rappels frame : %.1f i/s (attendu ~60)", fps);

    /* 5. Changement de taille décidé par le client ; le tampon remplacé est rendu. */
    struct buf *last = &b[cur ^ 1];
    draw(&b[2], W2 / 2);
    wl_surface_attach(a.surf, b[2].wb, 0, 0);
    wl_surface_damage_buffer(a.surf, 0, 0, W2, H2);
    b[2].busy = 1;
    wl_surface_commit(a.surf);
    wl_display_roundtrip(a.d);
    CHECK(!last->busy && b[2].busy && wl_display_get_error(a.d) == 0, "nouvelle taille %dx%d, tampon remplace rendu", W2, H2);

    if (hold > 0) {
        printf("wltest : fenetre affichee %d s\n", hold);
        fflush(stdout);
        double end = now_ms() + hold * 1000.0;
        while (now_ms() < end && !a.closed) pump(a.d, &a.closed, (int)(end - now_ms()));
    }

    xdg_toplevel_destroy(a.top);
    xdg_surface_destroy(a.xs);
    wl_surface_destroy(a.surf);
    for (int i = 0; i < 3; i++) wl_buffer_destroy(b[i].wb);
    wl_shm_pool_destroy(a.wpool);
    wl_display_roundtrip(a.d);
    CHECK(wl_display_get_error(a.d) == 0, "destruction propre de la fenetre");
    teardown(&a);

    /* 6. Erreurs de protocole attendues. */
    if (connect_app(&a) == 0 && open_window(&a, "erreur") == 0) {
        xdg_surface_ack_configure(a.xs, a.serial + 1000);
        expect_error(&a, "ack_configure inconnu", "xdg_surface", XDG_SURFACE_ERROR_INVALID_SERIAL);
    } else {
        CHECK(0, "connexion pour l'erreur ack_configure");
    }
    teardown(&a);

    if (connect_app(&a) == 0 && make_pool(&a, 4096) == 0) {
        wl_shm_pool_create_buffer(a.wpool, 0, 64, 8, 64, WL_SHM_FORMAT_XRGB8888);
        expect_error(&a, "pas de ligne trop petit", "wl_shm_pool", WL_SHM_ERROR_INVALID_STRIDE);
    } else {
        CHECK(0, "connexion pour l'erreur de pas");
    }
    teardown(&a);

    if (connect_app(&a) == 0 && make_pool(&a, 64 * 64 * 4) == 0) {
        struct buf e;
        make_buf(&a, &e, 0, 64, 64);
        a.surf = wl_compositor_create_surface(a.comp);
        a.xs = xdg_wm_base_get_xdg_surface(a.wm, a.surf);
        a.top = xdg_surface_get_toplevel(a.xs);
        wl_surface_attach(a.surf, e.wb, 0, 0);
        wl_surface_commit(a.surf);
        expect_error(&a, "tampon avant configuration", "xdg_surface", XDG_SURFACE_ERROR_UNCONFIGURED_BUFFER);
    } else {
        CHECK(0, "connexion pour l'erreur de tampon");
    }
    teardown(&a);

    /* 7. Connexions simultanées. */
    struct wl_display *many[32];
    int good = 0;
    for (int i = 0; i < 32; i++) many[i] = wl_display_connect(NULL);
    for (int i = 0; i < 32; i++) good += many[i] && wl_display_roundtrip(many[i]) >= 0;
    for (int i = 0; i < 32; i++)
        if (many[i]) wl_display_disconnect(many[i]);
    CHECK(good == 32, "32 connexions simultanees (%d repondent)", good);

    /* 8. Octets aléatoires sur une socket brute : le serveur coupe, puis répond toujours. */
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un sa = {.sun_family = AF_UNIX};
    snprintf(sa.sun_path, sizeof sa.sun_path, "%s/%s", getenv("XDG_RUNTIME_DIR"), getenv("WAYLAND_DISPLAY"));
    int closed = 0;
    if (s >= 0 && connect(s, (struct sockaddr *)&sa, sizeof sa) == 0) {
        uint32_t junk[1024];
        uint64_t x = 0x9e3779b97f4a7c15ull;
        for (int i = 0; i < 1024; i++) {
            x ^= x << 13, x ^= x >> 7, x ^= x << 17;
            junk[i] = (uint32_t)x;
        }
        (void)!write(s, junk, sizeof junk);
        char tmp[4096];
        struct pollfd p = {s, POLLIN, 0};
        for (int k = 0; k < 64 && poll(&p, 1, 2000) > 0; k++) {
            ssize_t got = read(s, tmp, sizeof tmp);
            if (got <= 0) {
                closed = 1;
                break;
            }
        }
    }
    if (s >= 0) close(s);
    CHECK(closed, "octets aleatoires : connexion fermee par le serveur");
    r = connect_app(&a);
    CHECK(r == 0, "le serveur repond toujours apres les erreurs");
    teardown(&a);

    printf("wltest : FIN %d ok, %d echec\n", n_ok, n_ko);
    fflush(stdout);
    return n_ko ? 1 : 0;
}
