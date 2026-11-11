/*
 * wltime : banc de conformité de MaiLand (serveur Wayland du noyau de MaiOS), phase W3.
 *
 * Client libwayland-client de Debian (glibc dynamique), comme les vraies applications :
 *   - presentation-time : chaque image animée par les rappels frame reçoit `presented` avec un
 *     instant de CLOCK_MONOTONIC postérieur au commit, un numéro croissant, la période ; le
 *     rappel frame de la même image porte le même instant ;
 *   - mise à jour remplacée avant son image : `discarded` (barrière fifo pour l'ordonner) ;
 *   - fifo-v1 : des commits envoyés d'un coup sont présentés un par image, sans perte ;
 *   - commit-timing-v1 : jamais présenté avant l'instant demandé, au plus deux images après ;
 *   - viewporter + single-pixel-buffer + transformation et échelle du tampon : vérifiés au
 *     pixel près par le harnais sur des captures (marqueurs « wltime : pret capture … »,
 *     touche espace pour continuer) ;
 *   - fractional-scale : échelle préférée 120 (1,0) ;
 *   - erreurs de protocole attendues de chaque extension.
 *
 * Sortie : lignes [ok] / [ECHEC], puis « wltime : FIN <ok> ok, <echec> echec ».
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
#include "commit-timing-v1-client-protocol.h"
#include "fifo-v1-client-protocol.h"
#include "fractional-scale-v1-client-protocol.h"
#include "presentation-time-client-protocol.h"
#include "single-pixel-buffer-v1-client-protocol.h"
#include "viewporter-client-protocol.h"
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

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* Délai d'attente du harnais (une capture prend quelques secondes). */
#define HARNESS_MS 30000

/* --- état --------------------------------------------------------------------------------- */

struct fb {
    struct wp_presentation_feedback *f;
    int state; /* 0 en attente, 1 présenté, 2 écarté */
    int syncs;
    uint64_t t, seq, commit;
    uint32_t refresh, flags;
};

struct app {
    struct wl_display *d;
    struct wl_registry *reg;
    struct wl_compositor *comp;
    struct wl_shm *shm;
    struct xdg_wm_base *wm;
    struct wl_output *out;
    struct wl_seat *seat;
    struct wl_keyboard *kbd;
    struct wp_presentation *pres;
    struct wp_fifo_manager_v1 *fifo_mgr;
    struct wp_commit_timing_manager_v1 *timing_mgr;
    struct wp_viewporter *vporter;
    struct wp_fractional_scale_manager_v1 *frac_mgr;
    struct wp_single_pixel_buffer_manager_v1 *spb;
    int clock_id, keys, preferred;
    struct wl_surface *surf;
    struct xdg_surface *xs;
    struct xdg_toplevel *top;
    uint32_t serial;
    int configured, cfg_w, cfg_h, maximized;
    int frame_done;
    uint32_t frame_ms;
    int fd;
    size_t pool_size;
    uint8_t *pool;
    struct wl_shm_pool *wpool;
};

/* --- auditeurs --------------------------------------------------------------------------- */

static void wm_ping(void *d, struct xdg_wm_base *wm, uint32_t serial) {
    (void)d;
    xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = {wm_ping};

static void pres_clock(void *data, struct wp_presentation *p, uint32_t clk) {
    (void)p;
    ((struct app *)data)->clock_id = (int)clk;
}
static const struct wp_presentation_listener pres_listener = {pres_clock};

static void kbd_keymap(void *d, struct wl_keyboard *k, uint32_t f, int32_t fd, uint32_t s) {
    (void)d, (void)k, (void)f, (void)s;
    close(fd);
}
static void kbd_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *su, struct wl_array *a) {
    (void)d, (void)k, (void)s, (void)su, (void)a;
}
static void kbd_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *su) { (void)d, (void)k, (void)s, (void)su; }
static void kbd_key(void *data, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t state) {
    (void)k, (void)s, (void)t, (void)key;
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED) ((struct app *)data)->keys++;
}
static void kbd_mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t g) {
    (void)d, (void)k, (void)s, (void)a, (void)b, (void)c, (void)g;
}
static void kbd_repeat(void *d, struct wl_keyboard *k, int32_t r, int32_t de) { (void)d, (void)k, (void)r, (void)de; }
static const struct wl_keyboard_listener kbd_listener = {kbd_keymap, kbd_enter, kbd_leave, kbd_key, kbd_mods, kbd_repeat};

static void seat_caps(void *data, struct wl_seat *s, uint32_t caps) {
    struct app *a = data;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !a->kbd) {
        a->kbd = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(a->kbd, &kbd_listener, a);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *n) { (void)d, (void)s, (void)n; }
static const struct wl_seat_listener seat_listener = {seat_caps, seat_name};

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    struct app *a = data;
    if (!strcmp(iface, wl_compositor_interface.name)) {
        a->comp = wl_registry_bind(r, name, &wl_compositor_interface, MIN(v, 6));
    } else if (!strcmp(iface, wl_shm_interface.name)) {
        a->shm = wl_registry_bind(r, name, &wl_shm_interface, MIN(v, 1));
    } else if (!strcmp(iface, zxdg_decoration_manager_v1_interface.name)) {
        deco_mgr = wl_registry_bind(r, name, &zxdg_decoration_manager_v1_interface, 1);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, MIN(v, 6));
        xdg_wm_base_add_listener(a->wm, &wm_listener, a);
    } else if (!strcmp(iface, wl_output_interface.name) && !a->out) {
        a->out = wl_registry_bind(r, name, &wl_output_interface, 1);
    } else if (!strcmp(iface, wl_seat_interface.name) && !a->seat) {
        a->seat = wl_registry_bind(r, name, &wl_seat_interface, MIN(v, 5));
        wl_seat_add_listener(a->seat, &seat_listener, a);
    } else if (!strcmp(iface, wp_presentation_interface.name)) {
        a->pres = wl_registry_bind(r, name, &wp_presentation_interface, MIN(v, 2));
        wp_presentation_add_listener(a->pres, &pres_listener, a);
    } else if (!strcmp(iface, wp_fifo_manager_v1_interface.name)) {
        a->fifo_mgr = wl_registry_bind(r, name, &wp_fifo_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_commit_timing_manager_v1_interface.name)) {
        a->timing_mgr = wl_registry_bind(r, name, &wp_commit_timing_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_viewporter_interface.name)) {
        a->vporter = wl_registry_bind(r, name, &wp_viewporter_interface, 1);
    } else if (!strcmp(iface, wp_fractional_scale_manager_v1_interface.name)) {
        a->frac_mgr = wl_registry_bind(r, name, &wp_fractional_scale_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_single_pixel_buffer_manager_v1_interface.name)) {
        a->spb = wl_registry_bind(r, name, &wp_single_pixel_buffer_manager_v1_interface, 1);
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
    uint32_t *s;
    (void)t;
    a->cfg_w = w, a->cfg_h = h;
    a->maximized = 0;
    wl_array_for_each(s, states) if (*s == XDG_TOPLEVEL_STATE_MAXIMIZED) a->maximized = 1;
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d, (void)t; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) { (void)d, (void)t, (void)w, (void)h; }
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) { (void)d, (void)t, (void)c; }
static const struct xdg_toplevel_listener top_listener = {top_configure, top_close, top_bounds, top_caps};

static void frame_done(void *data, struct wl_callback *cb, uint32_t t) {
    struct app *a = data;
    a->frame_done = 1;
    a->frame_ms = t;
    wl_callback_destroy(cb);
}
static const struct wl_callback_listener frame_listener = {frame_done};

static void fb_sync(void *data, struct wp_presentation_feedback *f, struct wl_output *o) {
    (void)f, (void)o;
    ((struct fb *)data)->syncs++;
}
static void fb_presented(void *data, struct wp_presentation_feedback *f, uint32_t sec_hi, uint32_t sec_lo, uint32_t nsec,
                         uint32_t refresh, uint32_t seq_hi, uint32_t seq_lo, uint32_t flags) {
    struct fb *b = data;
    b->state = 1;
    b->t = (((uint64_t)sec_hi << 32) | sec_lo) * 1000000000ull + nsec;
    b->seq = ((uint64_t)seq_hi << 32) | seq_lo;
    b->refresh = refresh;
    b->flags = flags;
    wp_presentation_feedback_destroy(f);
    b->f = NULL;
}
static void fb_discarded(void *data, struct wp_presentation_feedback *f) {
    struct fb *b = data;
    b->state = 2;
    wp_presentation_feedback_destroy(f);
    b->f = NULL;
}
static const struct wp_presentation_feedback_listener fb_listener = {fb_sync, fb_presented, fb_discarded};

static void frac_preferred(void *data, struct wp_fractional_scale_v1 *f, uint32_t scale) {
    (void)f;
    ((struct app *)data)->preferred = (int)scale;
}
static const struct wp_fractional_scale_v1_listener frac_listener = {frac_preferred};

/* --- boucle ------------------------------------------------------------------------------ */

/* Distribue les événements jusqu'à ce que `cond` soit vraie ou le délai écoulé (-1). */
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

struct fbs {
    struct fb *v;
    int n;
};
static int fbs_settled(void *p) {
    struct fbs *s = p;
    for (int i = 0; i < s->n; i++)
        if (s->v[i].state == 0) return 0;
    return 1;
}
/* Attend que tous les retours aient reçu presented ou discarded. */
static int settle(struct app *a, struct fb *v, int n, int ms) {
    struct fbs s = {v, n};
    return pump_cond(a->d, fbs_settled, &s, ms);
}

static void feedback(struct app *a, struct fb *b) {
    memset(b, 0, sizeof *b);
    b->f = wp_presentation_feedback(a->pres, a->surf);
    wp_presentation_feedback_add_listener(b->f, &fb_listener, b);
}

static int connect_app(struct app *a) {
    memset(a, 0, sizeof *a);
    a->fd = -1;
    a->clock_id = -1;
    a->d = wl_display_connect(NULL);
    if (!a->d) return -1;
    a->reg = wl_display_get_registry(a->d);
    wl_registry_add_listener(a->reg, &reg_listener, a);
    if (wl_display_roundtrip(a->d) < 0 || wl_display_roundtrip(a->d) < 0) return -1;
    return a->comp && a->shm && a->wm ? 0 : -1;
}

static int make_pool(struct app *a, size_t size) {
    a->fd = memfd_create("wltime", MFD_CLOEXEC);
    if (a->fd < 0 || ftruncate(a->fd, (off_t)size) < 0) return -1;
    a->pool = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, a->fd, 0);
    if (a->pool == MAP_FAILED) return -1;
    a->pool_size = size;
    a->wpool = wl_shm_create_pool(a->shm, a->fd, (int32_t)size);
    return 0;
}

static struct wl_buffer *make_buf(struct app *a, size_t offset, int w, int h) {
    return wl_shm_pool_create_buffer(a->wpool, (int32_t)offset, w, h, w * 4, WL_SHM_FORMAT_XRGB8888);
}

static void fill(struct app *a, size_t offset, int w, int h, uint32_t color) {
    uint32_t *px = (uint32_t *)(a->pool + offset);
    for (int i = 0; i < w * h; i++) px[i] = color;
}

/* Tampon d'un pixel opaque de couleur 0xRRGGBB. */
static struct wl_buffer *solid(struct app *a, uint32_t rgb) {
    uint32_t r = (rgb >> 16) & 0xFF, g = (rgb >> 8) & 0xFF, b = rgb & 0xFF;
    return wp_single_pixel_buffer_manager_v1_create_u32_rgba_buffer(a->spb, r * 0x01010101u, g * 0x01010101u,
                                                                     b * 0x01010101u, 0xFFFFFFFFu);
}

static void teardown(struct app *a) {
    if (a->d) wl_display_disconnect(a->d);
    if (a->pool && a->pool != MAP_FAILED) munmap(a->pool, a->pool_size);
    if (a->fd >= 0) close(a->fd);
    memset(a, 0, sizeof *a);
}

static int open_window(struct app *a, const char *title) {
    a->surf = wl_compositor_create_surface(a->comp);
    a->xs = xdg_wm_base_get_xdg_surface(a->wm, a->surf);
    xdg_surface_add_listener(a->xs, &xs_listener, a);
    a->top = xdg_surface_get_toplevel(a->xs);
    xdg_toplevel_add_listener(a->top, &top_listener, a);
    xdg_toplevel_set_title(a->top, title);
    xdg_toplevel_set_app_id(a->top, "org.maios.wltime");
    server_side(a->top);
    wl_surface_commit(a->surf);
    int r = pump(a->d, &a->configured, 3000);
    xdg_surface_ack_configure(a->xs, a->serial);
    return r;
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

/* Attend une touche (envoyée par le harnais après sa capture). */
struct key_wait {
    struct app *a;
    int before;
};
static int key_came(void *p) {
    struct key_wait *k = p;
    return k->a->keys > k->before;
}
static void wait_key(struct app *a) {
    struct key_wait k = {a, a->keys};
    pump_cond(a->d, key_came, &k, HARNESS_MS);
}

/* Configuration reçue avec l'état agrandi. */
static int maximized_configured(void *p) {
    struct app *a = p;
    return a->configured && a->maximized;
}

/* Couleurs des captures (exactes, peu probables ailleurs à l'écran). */
#define RED 0xFF0000u
#define GREEN 0x00FF00u
#define BLUE 0x0000FFu
#define WHITE 0xFFFFFFu
#define MAGENTA 0xFF00FFu
#define SOLID 0x2080E0u

int main(void) {
    struct app a;
    printf("wltime : debut (pid %d)\n", getpid());
    fflush(stdout);

    /* 1. Globales. */
    int r = connect_app(&a);
    CHECK(r == 0 && a.pres && a.fifo_mgr && a.timing_mgr && a.vporter && a.frac_mgr && a.spb,
          "globales presentation, fifo, commit-timing, viewporter, fractional-scale, single-pixel-buffer");
    if (r || !a.pres || !a.fifo_mgr || !a.timing_mgr || !a.vporter || !a.frac_mgr || !a.spb) {
        printf("wltime : FIN %d ok, %d echec\n", n_ok, n_ko);
        return 1;
    }
    CHECK(a.clock_id == CLOCK_MONOTONIC, "wp_presentation.clock_id = CLOCK_MONOTONIC (recu %d)", a.clock_id);

    /* 2. Fenêtre, échelle préférée. */
    r = open_window(&a, "wltime - MaiLand W3");
    struct wp_fractional_scale_v1 *frac = wp_fractional_scale_manager_v1_get_fractional_scale(a.frac_mgr, a.surf);
    wp_fractional_scale_v1_add_listener(frac, &frac_listener, &a);
    wl_display_roundtrip(a.d);
    CHECK(r == 0 && a.preferred == 120, "fenetre configuree, echelle preferee %d/120", a.preferred);

    const int W = 320, H = 240;
    r = make_pool(&a, (size_t)W * H * 4 * 2 + 4096 + 64 * 4);
    struct wl_buffer *b[2] = {make_buf(&a, 0, W, H), make_buf(&a, (size_t)W * H * 4, W, H)};
    fill(&a, 0, W, H, 0xFF203040);
    fill(&a, (size_t)W * H * 4, W, H, 0xFF406080);

    /* 3. presentation-time sur une animation menée par les rappels frame. */
    enum { NF = 30 };
    struct fb fb[NF];
    uint32_t cb_ms[NF];
    for (int i = 0; i < NF; i++) {
        wl_surface_attach(a.surf, b[i & 1], 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, W, H);
        feedback(&a, &fb[i]);
        a.frame_done = 0;
        wl_callback_add_listener(wl_surface_frame(a.surf), &frame_listener, &a);
        fb[i].commit = now_ns();
        wl_surface_commit(a.surf);
        if (pump(a.d, &a.frame_done, 2000) < 0) break;
        cb_ms[i] = a.frame_ms;
    }
    settle(&a, fb, NF, 2000);
    int presented = 0, syncs = 0, order = 1, after = 1, same = 1, refresh = 1, zero = 0;
    uint64_t lat_max = 0;
    for (int i = 0; i < NF; i++) {
        if (fb[i].state != 1) continue;
        presented++;
        syncs += fb[i].syncs > 0;
        if (i && fb[i - 1].state == 1 && (fb[i].seq <= fb[i - 1].seq || fb[i].t <= fb[i - 1].t)) order = 0;
        if (fb[i].t < fb[i].commit) after = 0;
        if (fb[i].t - fb[i].commit > lat_max) lat_max = fb[i].t - fb[i].commit;
        int64_t d = (int64_t)cb_ms[i] - (int64_t)(fb[i].t / 1000000ull % (1ull << 32));
        if (d < -1 || d > 1) same = 0;
        if (fb[i].refresh < 16600000 || fb[i].refresh > 16700000) refresh = 0;
        zero += (fb[i].flags & WP_PRESENTATION_FEEDBACK_KIND_ZERO_COPY) != 0;
    }
    double span = presented > 1 ? (double)(fb[NF - 1].t - fb[0].t) / 1e6 / (NF - 1) : 0;
    printf("wltime : presentation : %d/%d presentees, intervalle moyen %.2f ms, latence max %.2f ms\n", presented, NF, span,
           (double)lat_max / 1e6);
    CHECK(presented == NF && syncs == NF, "presentation-time : %d/%d presentees, sync_output sur %d", presented, NF, syncs);
    CHECK(order && after, "instants et numeros croissants, toujours apres le commit (CLOCK_MONOTONIC)");
    CHECK(lat_max < 100000000ull, "latence commit -> presentation %.2f ms", (double)lat_max / 1e6);
    CHECK(same, "le rappel frame porte l'instant de la presentation");
    CHECK(refresh && span > 14.0 && span < 25.0, "periode annoncee ~16,67 ms, intervalle moyen %.2f ms", span);
    CHECK(zero == NF, "tampons montres tels quels : composition directe (zero_copy sur %d/%d)", zero, NF);

    /* 4. Mise à jour remplacée avant son image : discarded. X pose une barrière ; A et B
     *    l'attendent, puis sont appliquées ensemble : A ne sera jamais montrée. */
    struct wp_fifo_v1 *fifo = wp_fifo_manager_v1_get_fifo(a.fifo_mgr, a.surf);
    struct fb x, ab[2];
    wl_surface_attach(a.surf, b[0], 0, 0);
    wl_surface_damage_buffer(a.surf, 0, 0, W, H);
    feedback(&a, &x);
    wp_fifo_v1_set_barrier(fifo);
    wl_surface_commit(a.surf);
    for (int i = 0; i < 2; i++) {
        wl_surface_attach(a.surf, b[(i + 1) & 1], 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, W, H);
        feedback(&a, &ab[i]);
        wp_fifo_v1_wait_barrier(fifo);
        wl_surface_commit(a.surf);
    }
    settle(&a, &x, 1, 2000);
    settle(&a, ab, 2, 2000);
    CHECK(x.state == 1 && ab[0].state == 2 && ab[1].state == 1 && ab[1].seq > x.seq,
          "remplacee avant son image : discarded (X %d, A %d, B %d, numeros %llu -> %llu)", x.state, ab[0].state, ab[1].state,
          (unsigned long long)x.seq, (unsigned long long)ab[1].seq);

    /* 5. fifo : NQ commits d'un coup, chacun avec barrière, présentés un par image. */
    enum { NQ = 20 };
    struct wp_viewport *vp = wp_viewporter_get_viewport(a.vporter, a.surf);
    wp_viewport_set_destination(vp, W, H);
    struct wl_buffer *sb[NQ];
    struct fb q[NQ];
    for (int i = 0; i < NQ; i++) {
        sb[i] = solid(&a, 0x101010u * (uint32_t)(i % 15 + 1));
        wl_surface_attach(a.surf, sb[i], 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, 1, 1);
        feedback(&a, &q[i]);
        wp_fifo_v1_wait_barrier(fifo);
        wp_fifo_v1_set_barrier(fifo);
        wl_surface_commit(a.surf);
    }
    uint64_t t0 = now_ns();
    settle(&a, q, NQ, 5000);
    int qp = 0, qorder = 1, qcopy = 0;
    for (int i = 0; i < NQ; i++) {
        qp += q[i].state == 1;
        qcopy += q[i].state == 1 && !(q[i].flags & WP_PRESENTATION_FEEDBACK_KIND_ZERO_COPY);
        if (i && (q[i].state != 1 || q[i - 1].state != 1 || q[i].seq <= q[i - 1].seq)) qorder = 0;
    }
    double qspan = qp == NQ ? (double)(q[NQ - 1].t - q[0].t) / 1e6 : 0;
    printf("wltime : fifo : %d/%d presentees en %.1f ms (attente totale %.1f ms)\n", qp, NQ, qspan, (double)(now_ns() - t0) / 1e6);
    CHECK(qp == NQ && qorder, "fifo : %d/%d presentees, une par image (numeros strictement croissants)", qp, NQ);
    CHECK(qspan >= (NQ - 1) * 14.0, "fifo : chaque image tient au moins une periode (%.1f ms pour %d images)", qspan, NQ);
    CHECK(qcopy == NQ, "tampons d'un pixel etires : copies (sans zero_copy sur %d/%d)", qcopy, NQ);

    /* 6. commit-timing : jamais avant l'instant demandé. */
    struct wp_commit_timer_v1 *timer = wp_commit_timing_manager_v1_get_timer(a.timing_mgr, a.surf);
    struct fb t[2];
    uint64_t target[2];
    for (int i = 0; i < 2; i++) {
        target[i] = now_ns() + (i ? 150000000ull : 80000000ull);
        uint64_t sec = target[i] / 1000000000ull;
        wp_commit_timer_v1_set_timestamp(timer, (uint32_t)(sec >> 32), (uint32_t)sec, (uint32_t)(target[i] % 1000000000ull));
        wl_surface_attach(a.surf, solid(&a, i ? 0x808080u : 0x404040u), 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, 1, 1);
        feedback(&a, &t[i]);
        wl_surface_commit(a.surf);
    }
    settle(&a, t, 2, 3000);
    int ontime = 1;
    for (int i = 0; i < 2; i++) {
        double late = ((double)t[i].t - (double)target[i]) / 1e6;
        printf("wltime : commit-timing : visee +%d ms, presentee %+.2f ms apres\n", i ? 150 : 80, late);
        if (t[i].state != 1 || t[i].t < target[i] || late > 40.0) ontime = 0;
    }
    CHECK(ontime && t[1].seq > t[0].seq, "commit-timing : presentees apres l'instant vise, a moins de 40 ms");

    /* 7. Captures vérifiées au pixel près par le harnais, fenêtre agrandie. */
    wp_viewport_destroy(vp);
    wp_commit_timer_v1_destroy(timer);
    wp_fifo_v1_destroy(fifo);
    a.configured = 0;
    xdg_toplevel_set_maximized(a.top);
    pump_cond(a.d, maximized_configured, &a, 3000);
    xdg_surface_ack_configure(a.xs, a.serial);
    int MW = a.cfg_w & ~1, MH = a.cfg_h & ~1;
    CHECK(a.maximized && MW > 0 && MH > 0, "fenetre agrandie %dx%d", MW, MH);

    /* 7a. Transformation 90 et échelle 2 : tampon (2 MH) x (2 MW), quadrants à la surface. */
    size_t big = (size_t)MW * MH * 16;
    struct app pool2 = a;
    pool2.pool = NULL;
    if (make_pool(&pool2, big + 64) == 0) {
        int bw = 2 * MH, bh = 2 * MW;
        uint32_t *px = (uint32_t *)pool2.pool;
        for (int by = 0; by < bh; by++) {
            for (int bx = 0; bx < bw; bx++) {
                /* Weston, transformation 90 : bx = 2 sy, by = 2 (W - sx). */
                int sy = bx / 2, sx = MW - 1 - by / 2;
                uint32_t c = sx < MW / 2 ? (sy < MH / 2 ? RED : BLUE) : (sy < MH / 2 ? GREEN : WHITE);
                px[(size_t)by * bw + bx] = 0xFF000000u | c;
            }
        }
        struct wl_buffer *rb = wl_shm_pool_create_buffer(pool2.wpool, 0, bw, bh, bw * 4, WL_SHM_FORMAT_XRGB8888);
        wl_surface_set_buffer_transform(a.surf, WL_OUTPUT_TRANSFORM_90);
        wl_surface_set_buffer_scale(a.surf, 2);
        wl_surface_attach(a.surf, rb, 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, bw, bh);
        struct fb c;
        feedback(&a, &c);
        wl_surface_commit(a.surf);
        settle(&a, &c, 1, 3000);
        CHECK(c.state == 1 && wl_display_get_error(a.d) == 0, "tampon tourne de 90 a l'echelle 2 presente");
        printf("wltime : pret capture rotation %d %d 0,0,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X "
               "%d,%d,%06X %d,%d,%06X %d,%d,%06X\n",
               MW, MH, RED, MW / 4, MH / 4, RED, 3 * MW / 4, MH / 4, GREEN, MW / 4, 3 * MH / 4, BLUE, 3 * MW / 4, 3 * MH / 4, WHITE,
               MW / 2 - 1, MH / 4, RED, MW / 2, MH / 4, GREEN, MW / 4, MH / 2 - 1, RED, MW / 4, MH / 2, BLUE, MW - 1, MH - 1, WHITE);
        fflush(stdout);
        wait_key(&a);
    } else {
        CHECK(0, "pool de %zu octets pour la rotation", big);
    }

    /* 7b. Recadrage d'un bloc 2x2 d'un tampon 4x4, étiré à toute la fenêtre : aucune fuite
     *     de couleur des blocs voisins (le filtre reste dans la source). */
    {
        size_t off = (size_t)W * H * 8;
        uint32_t *px = (uint32_t *)(a.pool + off);
        for (int y = 0; y < 4; y++)
            for (int x = 0; x < 4; x++)
                px[y * 4 + x] = 0xFF000000u | (x < 2 ? (y < 2 ? RED : GREEN) : (y < 2 ? BLUE : MAGENTA));
        struct wl_buffer *cb = wl_shm_pool_create_buffer(a.wpool, (int32_t)off, 4, 4, 16, WL_SHM_FORMAT_XRGB8888);
        vp = wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewport_set_source(vp, wl_fixed_from_int(2), wl_fixed_from_int(2), wl_fixed_from_int(2), wl_fixed_from_int(2));
        wp_viewport_set_destination(vp, MW, MH);
        wl_surface_set_buffer_transform(a.surf, WL_OUTPUT_TRANSFORM_NORMAL);
        wl_surface_set_buffer_scale(a.surf, 1);
        wl_surface_attach(a.surf, cb, 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, 4, 4);
        struct fb c;
        feedback(&a, &c);
        wl_surface_commit(a.surf);
        settle(&a, &c, 1, 3000);
        CHECK(c.state == 1 && wl_display_get_error(a.d) == 0, "recadrage 2x2 etire a %dx%d presente", MW, MH);
        printf("wltime : pret capture recadrage %d %d 0,0,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X\n", MW, MH, MAGENTA,
               MW - 1, 0, MAGENTA, 0, MH - 1, MAGENTA, MW - 1, MH - 1, MAGENTA, MW / 2, MH / 2, MAGENTA);
        fflush(stdout);
        wait_key(&a);
    }

    /* 7c. Tampon d'un pixel étiré à toute la fenêtre. */
    {
        wp_viewport_set_source(vp, wl_fixed_from_int(-1), wl_fixed_from_int(-1), wl_fixed_from_int(-1), wl_fixed_from_int(-1));
        wl_surface_attach(a.surf, solid(&a, SOLID), 0, 0);
        wl_surface_damage_buffer(a.surf, 0, 0, 1, 1);
        struct fb c;
        feedback(&a, &c);
        wl_surface_commit(a.surf);
        settle(&a, &c, 1, 3000);
        CHECK(c.state == 1 && wl_display_get_error(a.d) == 0, "tampon d'un pixel etire a %dx%d presente", MW, MH);
        printf("wltime : pret capture pixel %d %d 0,0,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X %d,%d,%06X\n", MW, MH, SOLID, MW - 1, 0,
               SOLID, 0, MH - 1, SOLID, MW - 1, MH - 1, SOLID, MW / 2, MH / 2, SOLID);
        fflush(stdout);
        wait_key(&a);
    }
    wl_display_roundtrip(a.d);
    CHECK(wl_display_get_error(a.d) == 0, "aucune erreur de protocole sur le parcours normal");
    teardown(&a);

    /* 8. Erreurs de protocole attendues. */
#define ERRCASE(what, iface, code, body)                         \
    do {                                                         \
        if (connect_app(&a) == 0 && make_pool(&a, 65536) == 0) { \
            a.surf = wl_compositor_create_surface(a.comp);       \
            body;                                                \
            expect_error(&a, what, iface, code);                 \
        } else {                                                 \
            CHECK(0, "connexion pour %s", what);                 \
        }                                                        \
        teardown(&a);                                            \
    } while (0)

    ERRCASE("destination de viewport nulle", "wp_viewport", WP_VIEWPORT_ERROR_BAD_VALUE, {
        struct wp_viewport *v = wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewport_set_destination(v, 0, 5);
    });
    ERRCASE("source de viewport negative", "wp_viewport", WP_VIEWPORT_ERROR_BAD_VALUE, {
        struct wp_viewport *v = wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewport_set_source(v, wl_fixed_from_int(-1), 0, wl_fixed_from_int(4), wl_fixed_from_int(4));
    });
    ERRCASE("source non entiere sans destination", "wp_viewport", WP_VIEWPORT_ERROR_BAD_SIZE, {
        struct wp_viewport *v = wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewport_set_source(v, 0, 0, wl_fixed_from_double(1.5), wl_fixed_from_int(1));
        wl_surface_attach(a.surf, make_buf(&a, 0, 16, 16), 0, 0);
        wl_surface_commit(a.surf);
    });
    ERRCASE("source hors du tampon", "wp_viewport", WP_VIEWPORT_ERROR_OUT_OF_BUFFER, {
        struct wp_viewport *v = wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewport_set_source(v, wl_fixed_from_int(8), 0, wl_fixed_from_int(16), wl_fixed_from_int(4));
        wl_surface_attach(a.surf, make_buf(&a, 0, 16, 16), 0, 0);
        wl_surface_commit(a.surf);
    });
    ERRCASE("deux viewports", "wp_viewporter", WP_VIEWPORTER_ERROR_VIEWPORT_EXISTS, {
        wp_viewporter_get_viewport(a.vporter, a.surf);
        wp_viewporter_get_viewport(a.vporter, a.surf);
    });
    ERRCASE("viewport d'une surface detruite", "wp_viewport", WP_VIEWPORT_ERROR_NO_SURFACE, {
        struct wp_viewport *v = wp_viewporter_get_viewport(a.vporter, a.surf);
        wl_surface_destroy(a.surf);
        wp_viewport_set_destination(v, 8, 8);
    });
    ERRCASE("deux objets fifo", "wp_fifo_manager_v1", WP_FIFO_MANAGER_V1_ERROR_ALREADY_EXISTS, {
        wp_fifo_manager_v1_get_fifo(a.fifo_mgr, a.surf);
        wp_fifo_manager_v1_get_fifo(a.fifo_mgr, a.surf);
    });
    ERRCASE("barriere d'une surface detruite", "wp_fifo_v1", WP_FIFO_V1_ERROR_SURFACE_DESTROYED, {
        struct wp_fifo_v1 *f = wp_fifo_manager_v1_get_fifo(a.fifo_mgr, a.surf);
        wl_surface_destroy(a.surf);
        wp_fifo_v1_set_barrier(f);
    });
    ERRCASE("tv_nsec invalide", "wp_commit_timer_v1", WP_COMMIT_TIMER_V1_ERROR_INVALID_TIMESTAMP, {
        struct wp_commit_timer_v1 *tm = wp_commit_timing_manager_v1_get_timer(a.timing_mgr, a.surf);
        wp_commit_timer_v1_set_timestamp(tm, 0, 1, 1000000000u);
    });
    ERRCASE("deux instants pour un commit", "wp_commit_timer_v1", WP_COMMIT_TIMER_V1_ERROR_TIMESTAMP_EXISTS, {
        struct wp_commit_timer_v1 *tm = wp_commit_timing_manager_v1_get_timer(a.timing_mgr, a.surf);
        wp_commit_timer_v1_set_timestamp(tm, 0, 1, 0);
        wp_commit_timer_v1_set_timestamp(tm, 0, 2, 0);
    });
    ERRCASE("deux minuteurs", "wp_commit_timing_manager_v1", WP_COMMIT_TIMING_MANAGER_V1_ERROR_COMMIT_TIMER_EXISTS, {
        wp_commit_timing_manager_v1_get_timer(a.timing_mgr, a.surf);
        wp_commit_timing_manager_v1_get_timer(a.timing_mgr, a.surf);
    });
    ERRCASE("deux objets d'echelle fractionnaire", "wp_fractional_scale_manager_v1",
            WP_FRACTIONAL_SCALE_MANAGER_V1_ERROR_FRACTIONAL_SCALE_EXISTS, {
                wp_fractional_scale_manager_v1_get_fractional_scale(a.frac_mgr, a.surf);
                wp_fractional_scale_manager_v1_get_fractional_scale(a.frac_mgr, a.surf);
            });
    ERRCASE("tampon non multiple de l'echelle", "wl_surface", WL_SURFACE_ERROR_INVALID_SIZE, {
        wl_surface_set_buffer_scale(a.surf, 2);
        wl_surface_attach(a.surf, make_buf(&a, 0, 15, 16), 0, 0);
        wl_surface_commit(a.surf);
    });

    printf("wltime : FIN %d ok, %d echec\n", n_ok, n_ko);
    fflush(stdout);
    return n_ko ? 1 : 0;
}
