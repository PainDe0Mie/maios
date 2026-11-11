/*
 * wlsub : banc de conformité de MaiLand (serveur Wayland du noyau de MaiOS), phase W4 :
 * sous-surfaces (wl_subcompositor), qu'utilisent Firefox, GTK, Qt et foot.
 *
 * Client libwayland-client de Debian. Chaque état est vérifié au pixel près par le harnais sur
 * une capture (marqueurs « wlsub : pret capture <nom> <L> <H> <points> », espace pour
 * continuer) :
 *   - mode synchronisé : le commit d'un enfant attend celui du parent ;
 *   - ordre : un nouvel enfant au sommet, puis place_below ;
 *   - mode désynchronisé : le commit de l'enfant s'affiche seul (et il est présenté) ;
 *   - enfant translucide (ARGB8888) mélangé sur le parent ;
 *   - set_position appliquée avec le parent ;
 *   - pointeur : wl_pointer.enter sur l'enfant sous lui, en coordonnées de l'enfant ; région
 *     d'entrée vide : le pointeur passe au parent (« wlsub : pret pointeur X Y ») ;
 *   - wl_subsurface.destroy : l'enfant disparaît ;
 *   - xdg-activation : jeton reçu pour la fenêtre qui a le focus, activation acceptée ;
 *   - erreurs bad_surface et bad_parent.
 *
 * Sortie : lignes [ok] / [ECHEC], puis « wlsub : FIN <ok> ok, <echec> echec ».
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include "presentation-time-client-protocol.h"
#include "xdg-activation-v1-client-protocol.h"
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

#define MIN(x, y) ((x) < (y) ? (x) : (y))
#define HARNESS_MS 30000

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

struct app {
    struct wl_display *d;
    struct wl_registry *reg;
    struct wl_compositor *comp;
    struct wl_subcompositor *subcomp;
    struct wl_shm *shm;
    struct xdg_wm_base *wm;
    struct wl_seat *seat;
    struct wl_keyboard *kbd;
    struct wl_pointer *ptr;
    struct wp_presentation *pres;
    struct xdg_activation_v1 *act;
    int keys, token_done;
    uint32_t key_serial;
    char token[128];
    struct wl_surface *surf;
    struct xdg_surface *xs;
    struct xdg_toplevel *top;
    uint32_t serial;
    int configured;
    /* Pointeur : surface et point de la dernière entrée ou du dernier mouvement. */
    struct wl_surface *ptr_surf;
    int px, py, enters;
    int fd;
    size_t pool_size, used;
    uint8_t *pool;
    struct wl_shm_pool *wpool;
};

/* --- auditeurs --------------------------------------------------------------------------- */

static void wm_ping(void *d, struct xdg_wm_base *wm, uint32_t serial) {
    (void)d;
    xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = {wm_ping};

static void kbd_keymap(void *d, struct wl_keyboard *k, uint32_t f, int32_t fd, uint32_t s) {
    (void)d, (void)k, (void)f, (void)s;
    close(fd);
}
static void kbd_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *su, struct wl_array *a) {
    (void)d, (void)k, (void)s, (void)su, (void)a;
}
static void kbd_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *su) { (void)d, (void)k, (void)s, (void)su; }
static void kbd_key(void *data, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t state) {
    struct app *a = data;
    (void)k, (void)t, (void)key;
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED) a->keys++, a->key_serial = s;
}
static void kbd_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t g) {
    (void)d, (void)k, (void)s, (void)a, (void)b, (void)c, (void)g;
}
static void kbd_repeat(void *d, struct wl_keyboard *k, int32_t r, int32_t de) { (void)d, (void)k, (void)r, (void)de; }
static const struct wl_keyboard_listener kbd_listener = {kbd_keymap, kbd_enter, kbd_leave, kbd_key, kbd_mods, kbd_repeat};

static void ptr_enter(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s, wl_fixed_t x, wl_fixed_t y) {
    struct app *a = data;
    (void)p, (void)serial;
    a->ptr_surf = s;
    a->px = wl_fixed_to_int(x), a->py = wl_fixed_to_int(y);
    a->enters++;
}
static void ptr_leave(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s) {
    struct app *a = data;
    (void)p, (void)serial;
    if (a->ptr_surf == s) a->ptr_surf = NULL;
}
static void ptr_motion(void *data, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y) {
    struct app *a = data;
    (void)p, (void)t;
    a->px = wl_fixed_to_int(x), a->py = wl_fixed_to_int(y);
}
static void ptr_button(void *d, struct wl_pointer *p, uint32_t s, uint32_t t, uint32_t b, uint32_t st) {
    (void)d, (void)p, (void)s, (void)t, (void)b, (void)st;
}
static void ptr_axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t a, wl_fixed_t v) { (void)d, (void)p, (void)t, (void)a, (void)v; }
static void ptr_frame(void *d, struct wl_pointer *p) { (void)d, (void)p; }
static void ptr_axis_source(void *d, struct wl_pointer *p, uint32_t s) { (void)d, (void)p, (void)s; }
static void ptr_axis_stop(void *d, struct wl_pointer *p, uint32_t t, uint32_t a) { (void)d, (void)p, (void)t, (void)a; }
static void ptr_axis_discrete(void *d, struct wl_pointer *p, uint32_t a, int32_t v) { (void)d, (void)p, (void)a, (void)v; }
static const struct wl_pointer_listener ptr_listener = {
    .enter = ptr_enter,
    .leave = ptr_leave,
    .motion = ptr_motion,
    .button = ptr_button,
    .axis = ptr_axis,
    .frame = ptr_frame,
    .axis_source = ptr_axis_source,
    .axis_stop = ptr_axis_stop,
    .axis_discrete = ptr_axis_discrete,
};

static void seat_caps(void *data, struct wl_seat *s, uint32_t caps) {
    struct app *a = data;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !a->kbd) {
        a->kbd = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(a->kbd, &kbd_listener, a);
    }
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !a->ptr) {
        a->ptr = wl_seat_get_pointer(s);
        wl_pointer_add_listener(a->ptr, &ptr_listener, a);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) { (void)d, (void)s, (void)n; }
static const struct wl_seat_listener seat_listener = {seat_caps, seat_name};

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    struct app *a = data;
    if (!strcmp(iface, wl_compositor_interface.name)) {
        a->comp = wl_registry_bind(r, name, &wl_compositor_interface, MIN(v, 6));
    } else if (!strcmp(iface, wl_subcompositor_interface.name)) {
        a->subcomp = wl_registry_bind(r, name, &wl_subcompositor_interface, 1);
    } else if (!strcmp(iface, wl_shm_interface.name)) {
        a->shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
    } else if (!strcmp(iface, zxdg_decoration_manager_v1_interface.name)) {
        deco_mgr = wl_registry_bind(r, name, &zxdg_decoration_manager_v1_interface, 1);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, MIN(v, 6));
        xdg_wm_base_add_listener(a->wm, &wm_listener, a);
    } else if (!strcmp(iface, wl_seat_interface.name) && !a->seat) {
        a->seat = wl_registry_bind(r, name, &wl_seat_interface, MIN(v, 5));
        wl_seat_add_listener(a->seat, &seat_listener, a);
    } else if (!strcmp(iface, wp_presentation_interface.name)) {
        a->pres = wl_registry_bind(r, name, &wp_presentation_interface, 1);
    } else if (!strcmp(iface, xdg_activation_v1_interface.name)) {
        a->act = wl_registry_bind(r, name, &xdg_activation_v1_interface, 1);
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

static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *s) {
    (void)d, (void)t, (void)w, (void)h, (void)s;
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d, (void)t; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) { (void)d, (void)t, (void)w, (void)h; }
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) { (void)d, (void)t, (void)c; }
static const struct xdg_toplevel_listener top_listener = {top_configure, top_close, top_bounds, top_caps};

static void token_done(void *data, struct xdg_activation_token_v1 *t, const char *token) {
    struct app *a = data;
    (void)t;
    snprintf(a->token, sizeof a->token, "%s", token);
    a->token_done = 1;
}
static const struct xdg_activation_token_v1_listener token_listener = {token_done};

/* Retour de présentation : 0 en attente, 1 présenté, 2 écarté. */
static void fb_sync(void *d, struct wp_presentation_feedback *f, struct wl_output *o) { (void)d, (void)f, (void)o; }
static void fb_presented(void *data, struct wp_presentation_feedback *f, uint32_t a, uint32_t b, uint32_t c, uint32_t r, uint32_t e,
                         uint32_t g, uint32_t flags) {
    (void)a, (void)b, (void)c, (void)r, (void)e, (void)g, (void)flags;
    *(int *)data = 1;
    wp_presentation_feedback_destroy(f);
}
static void fb_discarded(void *data, struct wp_presentation_feedback *f) {
    *(int *)data = 2;
    wp_presentation_feedback_destroy(f);
}
static const struct wp_presentation_feedback_listener fb_listener = {fb_sync, fb_presented, fb_discarded};

/* --- boucle ------------------------------------------------------------------------------ */

static int pump_cond(struct wl_display *d, int (*cond)(void *), void *arg, int ms) {
    uint64_t end = now_ns() + (uint64_t)ms * 1000000ull;
    while (!cond(arg)) {
        while (wl_display_prepare_read(d) != 0) wl_display_dispatch_pending(d);
        if (cond(arg)) {
            wl_display_cancel_read(d);
            break;
        }
        wl_display_flush(d);
        int64_t left = (int64_t)(end - now_ns()) / 1000000;
        struct pollfd p = {wl_display_get_fd(d), POLLIN, 0};
        if (left <= 0 || poll(&p, 1, (int)left) <= 0) {
            wl_display_cancel_read(d);
            return -1;
        }
        if (wl_display_read_events(d) < 0) return -1;
        if (wl_display_dispatch_pending(d) < 0) return -1;
    }
    return 0;
}

static int flag_set(void *p) { return *(volatile int *)p != 0; }
static int pump(struct wl_display *d, volatile int *flag, int ms) { return pump_cond(d, flag_set, (void *)flag, ms); }

struct key_wait {
    struct app *a;
    int before;
};
static int key_came(void *p) {
    struct key_wait *k = p;
    return k->a->keys > k->before;
}
/* Attend la touche du harnais (après sa capture ou son déplacement du pointeur). */
static void wait_key(struct app *a) {
    struct key_wait k = {a, a->keys};
    pump_cond(a->d, key_came, &k, HARNESS_MS);
}

/* Présentation d'un commit de `s` (retour demandé avant le commit). */
static int presented(struct app *a, struct wl_surface *s, int *state) {
    *state = 0;
    struct wp_presentation_feedback *f = wp_presentation_feedback(a->pres, s);
    wp_presentation_feedback_add_listener(f, &fb_listener, state);
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
    return a->comp && a->shm && a->wm && a->subcomp && a->pres ? 0 : -1;
}

static int make_pool(struct app *a, size_t size) {
    a->fd = memfd_create("wlsub", MFD_CLOEXEC);
    if (a->fd < 0 || ftruncate(a->fd, (off_t)size) < 0) return -1;
    a->pool = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, a->fd, 0);
    if (a->pool == MAP_FAILED) return -1;
    a->pool_size = size;
    a->wpool = wl_shm_create_pool(a->shm, a->fd, (int32_t)size);
    return 0;
}

/* Nouveau tampon plein de `argb` (ARGB8888 prémultiplié si `alpha`, XRGB8888 sinon). */
static struct wl_buffer *buffer(struct app *a, int w, int h, uint32_t argb, int alpha) {
    size_t size = (size_t)w * h * 4;
    if (a->used + size > a->pool_size) return NULL;
    uint32_t *px = (uint32_t *)(a->pool + a->used);
    for (int i = 0; i < w * h; i++) px[i] = argb;
    struct wl_buffer *b = wl_shm_pool_create_buffer(a->wpool, (int32_t)a->used, w, h, w * 4,
                                                    alpha ? WL_SHM_FORMAT_ARGB8888 : WL_SHM_FORMAT_XRGB8888);
    a->used += size;
    return b;
}

static void teardown(struct app *a) {
    if (a->d) wl_display_disconnect(a->d);
    if (a->pool && a->pool != MAP_FAILED) munmap(a->pool, a->pool_size);
    if (a->fd >= 0) close(a->fd);
    memset(a, 0, sizeof *a);
}

static void expect_error(struct app *a, const char *what, const char *iface, uint32_t code) {
    wl_display_roundtrip(a->d);
    const struct wl_interface *i = NULL;
    uint32_t id = 0;
    int err = wl_display_get_error(a->d);
    uint32_t c = err == EPROTO ? wl_display_get_protocol_error(a->d, &i, &id) : 0;
    CHECK(err == EPROTO && i && !strcmp(i->name, iface) && c == code, "%s : erreur %s.%u (recu errno %d, %s.%u)", what, iface,
          code, err, i ? i->name : "-", c);
}

/* Couleurs des captures. */
#define RED 0xFF0000u
#define GREEN 0x00FF00u
#define BLUE 0x0000FFu
#define WHITE 0xFFFFFFu
#define HALF 0x7F0000u /* noir à 50 % (0x80) sur rouge */
#define W 240
#define H 180

/* Capture : « wlsub : pret capture <nom> W H 0,0,ANCRE x,y,COULEUR ... » puis espace. */
static void capture(struct app *a, const char *name, const char *points) {
    wl_display_roundtrip(a->d);
    printf("wlsub : pret capture %s %d %d 0,0,%06X %s\n", name, W, H, RED, points);
    fflush(stdout);
    wait_key(a);
}

int main(void) {
    struct app a;
    char pts[512];
    int st;
    printf("wlsub : debut (pid %d)\n", getpid());
    fflush(stdout);

    int r = connect_app(&a);
    CHECK(r == 0, "globales wl_subcompositor, wl_compositor, wl_shm, xdg_wm_base, wp_presentation");
    if (r) {
        printf("wlsub : FIN %d ok, %d echec\n", n_ok, n_ko);
        return 1;
    }
    r = make_pool(&a, (size_t)8 << 20);

    /* Fenêtre rouge (parent). */
    a.surf = wl_compositor_create_surface(a.comp);
    a.xs = xdg_wm_base_get_xdg_surface(a.wm, a.surf);
    xdg_surface_add_listener(a.xs, &xs_listener, &a);
    a.top = xdg_surface_get_toplevel(a.xs);
    xdg_toplevel_add_listener(a.top, &top_listener, &a);
    xdg_toplevel_set_title(a.top, "wlsub - MaiLand W4");
    server_side(a.top);
    wl_surface_commit(a.surf);
    r |= pump(a.d, &a.configured, 3000);
    xdg_surface_ack_configure(a.xs, a.serial);
    wl_surface_attach(a.surf, buffer(&a, W, H, 0xFF000000u | RED, 0), 0, 0);
    wl_surface_damage_buffer(a.surf, 0, 0, W, H);
    presented(&a, a.surf, &st);
    wl_surface_commit(a.surf);
    pump(a.d, &st, 3000);
    CHECK(r == 0 && st == 1, "fenetre parente %dx%d presentee", W, H);

    /* 1. Mode synchronisé : l'enfant vert attend le commit du parent. */
    struct wl_surface *green = wl_compositor_create_surface(a.comp);
    struct wl_subsurface *sgreen = wl_subcompositor_get_subsurface(a.subcomp, green, a.surf);
    wl_subsurface_set_position(sgreen, 20, 30);
    wl_surface_attach(green, buffer(&a, 60, 40, 0xFF000000u | GREEN, 0), 0, 0);
    wl_surface_damage_buffer(green, 0, 0, 60, 40);
    wl_surface_commit(green);
    snprintf(pts, sizeof pts, "20,30,%06X 50,50,%06X 79,69,%06X", RED, RED, RED);
    capture(&a, "cache", pts);
    presented(&a, a.surf, &st);
    wl_surface_commit(a.surf);
    pump(a.d, &st, 3000);
    CHECK(st == 1, "commit du parent : enfant synchronise applique et presente");
    snprintf(pts, sizeof pts, "20,30,%06X 79,69,%06X 80,30,%06X 19,30,%06X 20,70,%06X", GREEN, GREEN, RED, RED, RED);
    capture(&a, "sync", pts);

    /* 2. Ordre : le bleu, créé après, est au-dessus du vert ; puis place_below. */
    struct wl_surface *blue = wl_compositor_create_surface(a.comp);
    struct wl_subsurface *sblue = wl_subcompositor_get_subsurface(a.subcomp, blue, a.surf);
    wl_subsurface_set_position(sblue, 60, 50);
    wl_surface_attach(blue, buffer(&a, 40, 40, 0xFF000000u | BLUE, 0), 0, 0);
    wl_surface_damage_buffer(blue, 0, 0, 40, 40);
    wl_surface_commit(blue);
    wl_surface_commit(a.surf);
    snprintf(pts, sizeof pts, "70,60,%06X 99,89,%06X 25,35,%06X", BLUE, BLUE, GREEN);
    capture(&a, "pile", pts);
    wl_subsurface_place_below(sblue, green);
    wl_surface_commit(a.surf);
    snprintf(pts, sizeof pts, "70,60,%06X 90,80,%06X 79,69,%06X 80,69,%06X", GREEN, BLUE, GREEN, BLUE);
    capture(&a, "ordre", pts);

    /* 3. Mode désynchronisé : le vert devient blanc sans commit du parent. */
    wl_subsurface_set_desync(sgreen);
    wl_surface_attach(green, buffer(&a, 60, 40, 0xFF000000u | WHITE, 0), 0, 0);
    wl_surface_damage_buffer(green, 0, 0, 60, 40);
    presented(&a, green, &st);
    wl_surface_commit(green);
    pump(a.d, &st, 3000);
    CHECK(st == 1, "enfant desynchronise presente sans commit du parent");
    snprintf(pts, sizeof pts, "25,35,%06X 70,60,%06X 90,80,%06X", WHITE, WHITE, BLUE);
    capture(&a, "desync", pts);

    /* 4. Enfant translucide : noir à 50 % sur le rouge. */
    struct wl_surface *glass = wl_compositor_create_surface(a.comp);
    struct wl_subsurface *sglass = wl_subcompositor_get_subsurface(a.subcomp, glass, a.surf);
    wl_subsurface_set_position(sglass, 150, 100);
    wl_surface_attach(glass, buffer(&a, 40, 40, 0x80000000u, 1), 0, 0);
    wl_surface_damage_buffer(glass, 0, 0, 40, 40);
    wl_surface_commit(glass);
    wl_surface_commit(a.surf);
    snprintf(pts, sizeof pts, "150,100,%06X 189,139,%06X 149,100,%06X 190,139,%06X", HALF, HALF, RED, RED);
    capture(&a, "alpha", pts);

    /* 5. Position : le blanc va en (100, 10) au commit du parent. */
    wl_subsurface_set_position(sgreen, 100, 10);
    wl_surface_commit(a.surf);
    snprintf(pts, sizeof pts, "25,35,%06X 100,10,%06X 159,49,%06X 99,10,%06X", RED, WHITE, WHITE, RED);
    capture(&a, "position", pts);

    /* 6. Pointeur : sur le blanc (100,10)-(160,50) en son point (10, 10) ; puis sur le bleu
     *    à région d'entrée vide (60,50)-(100,90) : le parent reçoit le pointeur. */
    if (a.ptr) {
        printf("wlsub : pret pointeur 110 20\n");
        fflush(stdout);
        wait_key(&a);
        wl_display_roundtrip(a.d);
        CHECK(a.ptr_surf == green && a.px == 10 && a.py == 10, "pointeur sur l'enfant, en (10, 10) de l'enfant (recu %s en %d,%d)",
              a.ptr_surf == green ? "l'enfant" : a.ptr_surf == a.surf ? "le parent" : "rien", a.px, a.py);
        struct wl_region *none = wl_compositor_create_region(a.comp);
        wl_surface_set_input_region(blue, none);
        wl_region_destroy(none);
        wl_surface_commit(blue);
        wl_surface_commit(a.surf);
        printf("wlsub : pret pointeur 70 80\n");
        fflush(stdout);
        wait_key(&a);
        wl_display_roundtrip(a.d);
        CHECK(a.ptr_surf == a.surf && a.px == 70 && a.py == 80,
              "region d'entree vide : pointeur au parent en (70, 80) (recu %s en %d,%d)",
              a.ptr_surf == blue ? "l'enfant" : a.ptr_surf == a.surf ? "le parent" : "rien", a.px, a.py);
    } else {
        CHECK(0, "pointeur du siege");
    }

    /* 6b. xdg-activation : jeton pour la fenêtre qui a le focus (numéro de la dernière touche
     *     du harnais), puis activation de la fenêtre avec lui. */
    if (a.act) {
        struct xdg_activation_token_v1 *t = xdg_activation_v1_get_activation_token(a.act);
        xdg_activation_token_v1_add_listener(t, &token_listener, &a);
        xdg_activation_token_v1_set_serial(t, a.key_serial, a.seat);
        xdg_activation_token_v1_set_surface(t, a.surf);
        xdg_activation_token_v1_commit(t);
        pump(a.d, &a.token_done, 3000);
        CHECK(a.token_done && strlen(a.token) >= 16, "jeton d'activation recu (%s)", a.token);
        xdg_activation_v1_activate(a.act, a.token, a.surf);
        xdg_activation_v1_activate(a.act, a.token, a.surf); /* deuxième usage : ignoré */
        xdg_activation_token_v1_destroy(t);
        wl_display_roundtrip(a.d);
        CHECK(wl_display_get_error(a.d) == 0, "activation de la fenetre avec le jeton");
    } else {
        CHECK(0, "globale xdg_activation_v1");
    }

    /* 7. Destruction : le blanc disparaît. */
    wl_subsurface_destroy(sgreen);
    wl_display_roundtrip(a.d);
    snprintf(pts, sizeof pts, "100,10,%06X 159,49,%06X 70,60,%06X", RED, RED, BLUE);
    capture(&a, "detruite", pts);
    wl_display_roundtrip(a.d);
    CHECK(wl_display_get_error(a.d) == 0, "aucune erreur de protocole sur le parcours normal");
    (void)sglass;
    teardown(&a);

    /* 8. Erreurs de protocole attendues. */
#define ERRCASE(what, iface, code, body)                       \
    do {                                                       \
        if (connect_app(&a) == 0) {                            \
            a.surf = wl_compositor_create_surface(a.comp);     \
            body;                                              \
            expect_error(&a, what, iface, code);               \
        } else {                                               \
            CHECK(0, "connexion pour %s", what);               \
        }                                                      \
        teardown(&a);                                          \
    } while (0)

    ERRCASE("surface parente d'elle-meme", "wl_subcompositor", WL_SUBCOMPOSITOR_ERROR_BAD_PARENT,
            { wl_subcompositor_get_subsurface(a.subcomp, a.surf, a.surf); });
    ERRCASE("boucle parent-enfant", "wl_subcompositor", WL_SUBCOMPOSITOR_ERROR_BAD_PARENT, {
        struct wl_surface *c = wl_compositor_create_surface(a.comp);
        wl_subcompositor_get_subsurface(a.subcomp, c, a.surf);
        wl_subcompositor_get_subsurface(a.subcomp, a.surf, c);
    });
    ERRCASE("surface qui a deja un role", "wl_subcompositor", WL_SUBCOMPOSITOR_ERROR_BAD_SURFACE, {
        struct wl_surface *p = wl_compositor_create_surface(a.comp);
        struct xdg_surface *x = xdg_wm_base_get_xdg_surface(a.wm, a.surf);
        xdg_surface_get_toplevel(x);
        wl_subcompositor_get_subsurface(a.subcomp, a.surf, p);
    });
    ERRCASE("place_above d'une surface etrangere", "wl_subsurface", WL_SUBSURFACE_ERROR_BAD_SURFACE, {
        struct wl_surface *c = wl_compositor_create_surface(a.comp);
        struct wl_surface *o = wl_compositor_create_surface(a.comp);
        struct wl_subsurface *s = wl_subcompositor_get_subsurface(a.subcomp, c, a.surf);
        wl_subsurface_place_above(s, o);
    });
    ERRCASE("jeton d'activation valide deux fois", "xdg_activation_token_v1", XDG_ACTIVATION_TOKEN_V1_ERROR_ALREADY_USED, {
        struct xdg_activation_token_v1 *t = xdg_activation_v1_get_activation_token(a.act);
        xdg_activation_token_v1_commit(t);
        xdg_activation_token_v1_commit(t);
    });

    printf("wlsub : FIN %d ok, %d echec\n", n_ok, n_ko);
    fflush(stdout);
    return n_ko ? 1 : 0;
}
