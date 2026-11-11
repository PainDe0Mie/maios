/*
 * wlinput : banc de conformité de MaiLand (serveur Wayland du noyau de MaiOS), phase W2.
 *
 * Client libwayland-client + libxkbcommon de Debian, piloté par le harnais (touches et souris
 * envoyées par le moniteur de QEMU quand le client affiche un marqueur « wlinput : pret … ») :
 *   - wl_seat : capacités, nom, carte XKB (memfd en lecture seule, compilée par libxkbcommon),
 *     répétition, touches evdev et modificateurs, entrée du clavier ;
 *   - fenêtre décorée par le serveur (xdg-decoration), agrandie (états maximized, activated) ;
 *   - wl_pointer : entrée, mouvement, boutons, molette (value120), trames ; curseur choisi par
 *     surface (wl_pointer.set_cursor) puis par forme (cursor-shape-v1) ;
 *   - menus xdg_popup : saisie refusée pour un numéro périmé, vrai menu contextuel (position,
 *     clavier pris par le menu, fermeture au clic dehors, clavier rendu à la fenêtre) ;
 *   - presse-papiers entre deux clients, puis persistance après la disparition de la source ;
 *   - xdg_wm_base.ping ;
 *   - erreurs de protocole attendues.
 *
 * Sortie : lignes [ok] / [ECHEC], puis « wlinput : FIN <ok> ok, <echec> echec ».
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "cursor-shape-v1-client-protocol.h"
#include "xdg-decoration-unstable-v1-client-protocol.h"
#include "xdg-shell-client-protocol.h"

/* Référencée par le code généré de cursor-shape-v1 (get_tablet_tool_v2), qu'on n'utilise pas. */
const struct wl_interface zwp_tablet_tool_v2_interface = {"zwp_tablet_tool_v2", 1, 0, NULL, 0, NULL};

/* Codes evdev (linux/input-event-codes.h). */
#define KEY_Q 16
#define KEY_A 30
#define KEY_LEFTSHIFT 42
#define BTN_LEFT 0x110
#define BTN_RIGHT 0x111

/* Attente d'une action du harnais (moniteur de QEMU, captures d'écran comprises). */
#define HARNESS_MS 30000

static int n_ok, n_ko;

/* Derniers événements du pointeur, affichés si une vérification de la souris échoue. */
static char trace[32][64];
static unsigned ntrace;
#define TRACE(...) snprintf(trace[ntrace++ % 32], 64, __VA_ARGS__)

static void dump_trace(void) {
    unsigned first = ntrace > 32 ? ntrace - 32 : 0;
    for (unsigned i = first; i < ntrace; i++) printf("wlinput :   %s\n", trace[i % 32]);
    fflush(stdout);
}

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

static void say(const char *m) {
    printf("wlinput : %s\n", m);
    fflush(stdout);
}

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

/* Offre du presse-papiers et ses types. */
struct offer {
    struct wl_data_offer *o;
    char mimes[512];
};

struct popup {
    struct wl_surface *surf;
    struct xdg_surface *xs;
    struct xdg_popup *p;
    uint32_t serial;
    int configured, x, y, w, h, done;
};

struct app {
    const char *name;
    struct wl_display *d;
    struct wl_registry *reg;
    struct wl_compositor *comp;
    struct wl_shm *shm;
    struct xdg_wm_base *wm;
    struct wl_output *out;
    struct wl_seat *seat;
    struct wl_data_device_manager *ddm;
    struct zxdg_decoration_manager_v1 *deco_mgr;
    struct wp_cursor_shape_manager_v1 *shape_mgr;
    int out_w, out_h, pings;
    uint32_t seat_caps;
    char seat_name[32];
    /* Fenêtre. */
    struct wl_surface *surf;
    struct xdg_surface *xs;
    struct xdg_toplevel *top;
    struct zxdg_toplevel_decoration_v1 *deco;
    uint32_t serial, deco_mode;
    int configured, cfg_w, cfg_h, maximized, activated, deco_configs, frame_done;
    /* Pool memfd. */
    int fd;
    size_t pool_size;
    uint8_t *pool;
    struct wl_shm_pool *wpool;
    /* Pointeur. */
    struct wl_pointer *ptr;
    struct wp_cursor_shape_device_v1 *shape_dev;
    struct wl_surface *cursor_surf, *ptr_surf, *shape_on;
    uint32_t enter_serial, btn_serial;
    int ix, iy, enters, leaves, motions, frames, axis_events, axis120, set_cursors, shapes;
    int left_down, left_up, right_down;
    /* Clavier. */
    struct wl_keyboard *kbd;
    struct xkb_context *xkb;
    struct xkb_keymap *keymap;
    struct xkb_state *state;
    struct wl_surface *kbd_surf;
    uint32_t key_serial;
    int keymap_fmt, keymap_ro, rate, delay, kbd_enters, kbd_leaves, down, shift_seen, ncodes;
    uint32_t codes[16];
    char text[64];
    /* Presse-papiers. */
    struct wl_data_device *dd;
    struct offer *sel;
    int sel_count;
    struct wl_data_source *src;
    const char *src_text;
    int src_sends, src_cancelled;
};

#define MIN(x, y) ((x) < (y) ? (x) : (y))

/* --- auditeurs : globales ---------------------------------------------------------------- */

static void out_geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t pw, int32_t ph, int32_t sp,
                         const char *make, const char *model, int32_t tr) {
    (void)d, (void)o, (void)x, (void)y, (void)pw, (void)ph, (void)sp, (void)make, (void)model, (void)tr;
}
static void out_mode(void *data, struct wl_output *o, uint32_t flags, int32_t w, int32_t h, int32_t r) {
    struct app *a = data;
    (void)o, (void)r;
    if (flags & WL_OUTPUT_MODE_CURRENT) a->out_w = w, a->out_h = h;
}
static void out_done(void *d, struct wl_output *o) { (void)d, (void)o; }
static void out_scale(void *d, struct wl_output *o, int32_t s) { (void)d, (void)o, (void)s; }
static void out_name(void *d, struct wl_output *o, const char *n) { (void)d, (void)o, (void)n; }
static void out_desc(void *d, struct wl_output *o, const char *s) { (void)d, (void)o, (void)s; }
static const struct wl_output_listener out_listener = {out_geometry, out_mode, out_done, out_scale, out_name, out_desc};

static void wm_ping(void *data, struct xdg_wm_base *wm, uint32_t serial) {
    ((struct app *)data)->pings++;
    xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = {wm_ping};

static void seat_caps(void *data, struct wl_seat *s, uint32_t caps) {
    (void)s;
    ((struct app *)data)->seat_caps = caps;
}
static void seat_name(void *data, struct wl_seat *s, const char *n) {
    (void)s;
    snprintf(((struct app *)data)->seat_name, 32, "%s", n);
}
static const struct wl_seat_listener seat_listener = {seat_caps, seat_name};

static void reg_global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
    struct app *a = data;
    if (!strcmp(iface, wl_compositor_interface.name)) {
        a->comp = wl_registry_bind(r, name, &wl_compositor_interface, MIN(v, 6));
    } else if (!strcmp(iface, wl_shm_interface.name)) {
        a->shm = wl_registry_bind(r, name, &wl_shm_interface, MIN(v, 2));
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, MIN(v, 6));
        xdg_wm_base_add_listener(a->wm, &wm_listener, a);
    } else if (!strcmp(iface, wl_output_interface.name) && !a->out) {
        a->out = wl_registry_bind(r, name, &wl_output_interface, MIN(v, 4));
        wl_output_add_listener(a->out, &out_listener, a);
    } else if (!strcmp(iface, wl_seat_interface.name) && !a->seat) {
        a->seat = wl_registry_bind(r, name, &wl_seat_interface, MIN(v, 9));
        wl_seat_add_listener(a->seat, &seat_listener, a);
    } else if (!strcmp(iface, wl_data_device_manager_interface.name)) {
        a->ddm = wl_registry_bind(r, name, &wl_data_device_manager_interface, MIN(v, 3));
    } else if (!strcmp(iface, zxdg_decoration_manager_v1_interface.name)) {
        a->deco_mgr = wl_registry_bind(r, name, &zxdg_decoration_manager_v1_interface, 1);
    } else if (!strcmp(iface, wp_cursor_shape_manager_v1_interface.name)) {
        a->shape_mgr = wl_registry_bind(r, name, &wp_cursor_shape_manager_v1_interface, MIN(v, 2));
    }
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d, (void)r, (void)n; }
static const struct wl_registry_listener reg_listener = {reg_global, reg_remove};

/* --- auditeurs : fenêtre ----------------------------------------------------------------- */

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
    a->maximized = a->activated = 0;
    wl_array_for_each(s, states) {
        if (*s == XDG_TOPLEVEL_STATE_MAXIMIZED) a->maximized = 1;
        if (*s == XDG_TOPLEVEL_STATE_ACTIVATED) a->activated = 1;
    }
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d, (void)t; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) { (void)d, (void)t, (void)w, (void)h; }
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) { (void)d, (void)t, (void)c; }
static const struct xdg_toplevel_listener top_listener = {top_configure, top_close, top_bounds, top_caps};

static void deco_configure(void *data, struct zxdg_toplevel_decoration_v1 *dd, uint32_t mode) {
    struct app *a = data;
    (void)dd;
    a->deco_mode = mode;
    a->deco_configs++;
}
static const struct zxdg_toplevel_decoration_v1_listener deco_listener = {deco_configure};

static void buf_release(void *data, struct wl_buffer *wb) {
    (void)wb;
    ((struct buf *)data)->busy = 0;
}
static const struct wl_buffer_listener buf_listener = {buf_release};

static void frame_done(void *data, struct wl_callback *cb, uint32_t t) {
    (void)t;
    ((struct app *)data)->frame_done = 1;
    wl_callback_destroy(cb);
}
static const struct wl_callback_listener frame_listener = {frame_done};

/* --- auditeurs : menus ------------------------------------------------------------------- */

static void pxs_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
    struct popup *p = data;
    (void)xs;
    p->serial = serial;
    p->configured++;
}
static const struct xdg_surface_listener pxs_listener = {pxs_configure};

static void popup_configure(void *data, struct xdg_popup *xp, int32_t x, int32_t y, int32_t w, int32_t h) {
    struct popup *p = data;
    (void)xp;
    p->x = x, p->y = y, p->w = w, p->h = h;
}
static void popup_done(void *data, struct xdg_popup *xp) {
    (void)xp;
    ((struct popup *)data)->done = 1;
}
static void popup_repositioned(void *d, struct xdg_popup *xp, uint32_t t) { (void)d, (void)xp, (void)t; }
static const struct xdg_popup_listener popup_listener = {popup_configure, popup_done, popup_repositioned};

/* --- auditeurs : pointeur ---------------------------------------------------------------- */

static void ptr_enter(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s, wl_fixed_t x, wl_fixed_t y) {
    struct app *a = data;
    a->enters++;
    a->ptr_surf = s;
    a->enter_serial = serial;
    a->ix = wl_fixed_to_int(x), a->iy = wl_fixed_to_int(y);
    TRACE("%s enter %s %d,%d", a->name, s == a->surf ? "fenetre" : "autre", a->ix, a->iy);
    if (s == a->surf && a->cursor_surf) {
        /* Curseur dessiné par le client. */
        wl_pointer_set_cursor(p, serial, a->cursor_surf, 4, 4);
        a->set_cursors++;
    } else if (s && s != a->surf && a->shape_dev) {
        /* Sur un menu : curseur choisi par sa forme, dessiné par le serveur. */
        wp_cursor_shape_device_v1_set_shape(a->shape_dev, serial, WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_POINTER);
        a->shape_on = s;
        a->shapes++;
    }
}
static void ptr_leave(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s) {
    struct app *a = data;
    (void)p, (void)serial;
    if (a->ptr_surf == s) a->ptr_surf = NULL;
    a->leaves++;
    TRACE("%s leave", a->name);
}
static void ptr_motion(void *data, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y) {
    struct app *a = data;
    (void)p, (void)t;
    a->motions++;
    a->ix = wl_fixed_to_int(x), a->iy = wl_fixed_to_int(y);
    TRACE("%s motion %d,%d", a->name, a->ix, a->iy);
}
static void ptr_button(void *data, struct wl_pointer *p, uint32_t serial, uint32_t t, uint32_t button, uint32_t state) {
    struct app *a = data;
    (void)p, (void)t;
    TRACE("%s button 0x%x %s", a->name, button, state == WL_POINTER_BUTTON_STATE_PRESSED ? "enfonce" : "relache");
    if (state == WL_POINTER_BUTTON_STATE_PRESSED) {
        a->btn_serial = serial;
        if (button == BTN_LEFT) a->left_down++;
        if (button == BTN_RIGHT) a->right_down++;
    } else if (button == BTN_LEFT) {
        a->left_up++;
    }
}
static void ptr_axis(void *data, struct wl_pointer *p, uint32_t t, uint32_t axis, wl_fixed_t v) {
    (void)p, (void)t;
    TRACE("%s axis %u %.1f", ((struct app *)data)->name, axis, wl_fixed_to_double(v));
    ((struct app *)data)->axis_events++;
}
static void ptr_frame(void *data, struct wl_pointer *p) {
    (void)p;
    ((struct app *)data)->frames++;
}
static void ptr_axis_source(void *d, struct wl_pointer *p, uint32_t s) { (void)d, (void)p, (void)s; }
static void ptr_axis_stop(void *d, struct wl_pointer *p, uint32_t t, uint32_t axis) { (void)d, (void)p, (void)t, (void)axis; }
static void ptr_axis_discrete(void *d, struct wl_pointer *p, uint32_t axis, int32_t v) { (void)d, (void)p, (void)axis, (void)v; }
static void ptr_axis_value120(void *data, struct wl_pointer *p, uint32_t axis, int32_t v) {
    (void)p, (void)axis;
    ((struct app *)data)->axis120 = v;
}
static void ptr_axis_dir(void *d, struct wl_pointer *p, uint32_t axis, uint32_t dir) { (void)d, (void)p, (void)axis, (void)dir; }
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
    .axis_value120 = ptr_axis_value120,
    .axis_relative_direction = ptr_axis_dir,
};

/* --- auditeurs : clavier ----------------------------------------------------------------- */

static void kbd_keymap(void *data, struct wl_keyboard *k, uint32_t format, int32_t fd, uint32_t size) {
    struct app *a = data;
    (void)k;
    a->keymap_fmt = (int)format;
    /* Un client ne doit pas pouvoir modifier la carte partagée par tous. */
    void *w = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    a->keymap_ro = w == MAP_FAILED;
    if (w != MAP_FAILED) munmap(w, size);
    char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (map == MAP_FAILED) return;
    if (!a->xkb || format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1) {
        munmap(map, size);
        return;
    }
    if (a->state) xkb_state_unref(a->state);
    if (a->keymap) xkb_keymap_unref(a->keymap);
    a->keymap = xkb_keymap_new_from_string(a->xkb, map, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    a->state = a->keymap ? xkb_state_new(a->keymap) : NULL;
    munmap(map, size);
}
static void kbd_enter(void *data, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s, struct wl_array *keys) {
    struct app *a = data;
    (void)k, (void)keys;
    a->kbd_surf = s;
    a->kbd_enters++;
    a->key_serial = serial;
}
static void kbd_leave(void *data, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s) {
    struct app *a = data;
    (void)k, (void)serial;
    if (a->kbd_surf == s) a->kbd_surf = NULL;
    a->kbd_leaves++;
}
static void kbd_key(void *data, struct wl_keyboard *k, uint32_t serial, uint32_t t, uint32_t key, uint32_t state) {
    struct app *a = data;
    (void)k, (void)t;
    a->key_serial = serial;
    if (state != WL_KEYBOARD_KEY_STATE_PRESSED) {
        if (a->down > 0) a->down--;
        return;
    }
    a->down++;
    if (a->ncodes < 16) a->codes[a->ncodes++] = key;
    if (a->state) {
        char s[16];
        int n = xkb_state_key_get_utf8(a->state, key + 8, s, sizeof s);
        if (n > 0 && strlen(a->text) + (size_t)n < sizeof a->text) strcat(a->text, s);
    }
}
static void kbd_modifiers(void *data, struct wl_keyboard *k, uint32_t serial, uint32_t dep, uint32_t lat, uint32_t lock,
                          uint32_t group) {
    struct app *a = data;
    (void)k, (void)serial;
    if (a->state) xkb_state_update_mask(a->state, dep, lat, lock, 0, 0, group);
    if (dep & 1) a->shift_seen = 1;
}
static void kbd_repeat(void *data, struct wl_keyboard *k, int32_t rate, int32_t delay) {
    struct app *a = data;
    (void)k;
    a->rate = rate, a->delay = delay;
}
static const struct wl_keyboard_listener kbd_listener = {kbd_keymap, kbd_enter, kbd_leave, kbd_key, kbd_modifiers, kbd_repeat};

/* --- auditeurs : presse-papiers ---------------------------------------------------------- */

static void offer_offer(void *data, struct wl_data_offer *o, const char *mime) {
    struct offer *of = data;
    (void)o;
    size_t n = strlen(of->mimes);
    if (n + strlen(mime) + 2 < sizeof of->mimes) {
        strcat(of->mimes, mime);
        strcat(of->mimes, "\n");
    }
}
static void offer_actions(void *d, struct wl_data_offer *o, uint32_t x) { (void)d, (void)o, (void)x; }
static void offer_action(void *d, struct wl_data_offer *o, uint32_t x) { (void)d, (void)o, (void)x; }
static const struct wl_data_offer_listener offer_listener = {offer_offer, offer_actions, offer_action};

static int has_mime(const struct offer *of, const char *mime) {
    char want[300];
    snprintf(want, sizeof want, "%s\n", mime);
    for (const char *p = of->mimes; (p = strstr(p, want)); p++)
        if (p == of->mimes || p[-1] == '\n') return 1;
    return 0;
}

static void dd_data_offer(void *data, struct wl_data_device *dd, struct wl_data_offer *o) {
    (void)data, (void)dd;
    struct offer *of = calloc(1, sizeof *of);
    of->o = o;
    wl_data_offer_add_listener(o, &offer_listener, of);
}
static void dd_enter(void *d, struct wl_data_device *dd, uint32_t s, struct wl_surface *sf, wl_fixed_t x, wl_fixed_t y,
                     struct wl_data_offer *o) {
    (void)d, (void)dd, (void)s, (void)sf, (void)x, (void)y, (void)o;
}
static void dd_leave(void *d, struct wl_data_device *dd) { (void)d, (void)dd; }
static void dd_motion(void *d, struct wl_data_device *dd, uint32_t t, wl_fixed_t x, wl_fixed_t y) {
    (void)d, (void)dd, (void)t, (void)x, (void)y;
}
static void dd_drop(void *d, struct wl_data_device *dd) { (void)d, (void)dd; }
static void dd_selection(void *data, struct wl_data_device *dd, struct wl_data_offer *o) {
    struct app *a = data;
    (void)dd;
    if (a->sel) {
        wl_data_offer_destroy(a->sel->o);
        free(a->sel);
    }
    a->sel = o ? wl_data_offer_get_user_data(o) : NULL;
    a->sel_count++;
}
static const struct wl_data_device_listener dd_listener = {dd_data_offer, dd_enter, dd_leave, dd_motion, dd_drop, dd_selection};

static void src_target(void *d, struct wl_data_source *s, const char *m) { (void)d, (void)s, (void)m; }
static void src_send(void *data, struct wl_data_source *s, const char *mime, int32_t fd) {
    struct app *a = data;
    (void)s, (void)mime;
    a->src_sends++;
    if (a->src_text) (void)!write(fd, a->src_text, strlen(a->src_text));
    close(fd);
}
static void src_cancelled(void *data, struct wl_data_source *s) {
    (void)s;
    ((struct app *)data)->src_cancelled++;
}
static void src_dnd_drop(void *d, struct wl_data_source *s) { (void)d, (void)s; }
static void src_dnd_finished(void *d, struct wl_data_source *s) { (void)d, (void)s; }
static void src_action(void *d, struct wl_data_source *s, uint32_t x) { (void)d, (void)s, (void)x; }
static const struct wl_data_source_listener src_listener = {src_target, src_send, src_cancelled, src_dnd_drop, src_dnd_finished,
                                                            src_action};

/* --- boucle ------------------------------------------------------------------------------ */

/*
 * Un pas de distribution sur une ou deux connexions (et un descripteur de plus, `xfd`, pour
 * attendre aussi un tube) : distribue ce qui est déjà lu, sinon attend jusqu'à `end`.
 * -1 : délai dépassé ou connexion en erreur.
 */
static int step(struct wl_display *d1, struct wl_display *d2, int xfd, double end) {
    struct wl_display *ds[2] = {d1, d2};
    struct pollfd p[3];
    int n = 0, busy = 0;
    for (int i = 0; i < 2; i++) {
        if (!ds[i]) continue;
        if (wl_display_get_error(ds[i])) return -1;
        int k = wl_display_dispatch_pending(ds[i]);
        if (k < 0) return -1;
        busy += k;
        wl_display_flush(ds[i]);
    }
    if (busy) return 0;
    for (int i = 0; i < 2; i++)
        if (ds[i]) p[n++] = (struct pollfd){wl_display_get_fd(ds[i]), POLLIN, 0};
    if (xfd >= 0) p[n++] = (struct pollfd){xfd, POLLIN, 0};
    int left = (int)(end - now_ms());
    if (left <= 0) return -1;
    int r = poll(p, n, left);
    if (r < 0) return errno == EINTR ? 0 : -1;
    if (r == 0) return -1;
    for (int i = 0, j = 0; i < 2; i++) {
        if (!ds[i]) continue;
        if (p[j++].revents & (POLLIN | POLLHUP | POLLERR)) {
            if (wl_display_prepare_read(ds[i]) == 0 && wl_display_read_events(ds[i]) < 0) return -1;
            if (wl_display_dispatch_pending(ds[i]) < 0) return -1;
        }
    }
    return 0;
}

/* Distribue les événements de `d1` (et `d2`) jusqu'à ce que `cond` soit vraie : 0, sinon -1. */
#define WAIT(d1, d2, cond, ms)                                     \
    ({                                                             \
        double end_ = now_ms() + (ms);                             \
        while (!(cond))                                            \
            if (step((d1), (d2), -1, end_) < 0) break;             \
        (cond) ? 0 : -1;                                           \
    })

static int connect_app(struct app *a, const char *name) {
    memset(a, 0, sizeof *a);
    a->name = name;
    a->fd = -1;
    a->d = wl_display_connect(NULL);
    if (!a->d) return -1;
    a->reg = wl_display_get_registry(a->d);
    wl_registry_add_listener(a->reg, &reg_listener, a);
    if (wl_display_roundtrip(a->d) < 0 || wl_display_roundtrip(a->d) < 0) return -1;
    return a->comp && a->shm && a->wm && a->seat ? 0 : -1;
}

/* Pointeur, clavier et presse-papiers. */
static void get_input(struct app *a) {
    a->xkb = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    a->ptr = wl_seat_get_pointer(a->seat);
    wl_pointer_add_listener(a->ptr, &ptr_listener, a);
    a->kbd = wl_seat_get_keyboard(a->seat);
    wl_keyboard_add_listener(a->kbd, &kbd_listener, a);
    if (a->shape_mgr) a->shape_dev = wp_cursor_shape_manager_v1_get_pointer(a->shape_mgr, a->ptr);
    if (a->ddm) {
        a->dd = wl_data_device_manager_get_data_device(a->ddm, a->seat);
        wl_data_device_add_listener(a->dd, &dd_listener, a);
    }
}

static int make_pool(struct app *a, size_t size) {
    a->fd = memfd_create("wlinput", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (a->fd < 0 || ftruncate(a->fd, (off_t)size) < 0) return -1;
    a->pool = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, a->fd, 0);
    if (a->pool == MAP_FAILED) return -1;
    a->pool_size = size;
    a->wpool = wl_shm_create_pool(a->shm, a->fd, (int32_t)size);
    return 0;
}

static void make_buf(struct app *a, struct buf *b, size_t offset, int w, int h, uint32_t format) {
    b->w = w, b->h = h, b->busy = 0;
    b->px = (uint32_t *)(a->pool + offset);
    b->wb = wl_shm_pool_create_buffer(a->wpool, (int32_t)offset, w, h, w * 4, format);
    wl_buffer_add_listener(b->wb, &buf_listener, b);
}

/* Dégradé, teinte `hue` (fenêtres). */
static void draw(struct buf *b, uint32_t hue) {
    for (int y = 0; y < b->h; y++) {
        uint32_t *row = b->px + (size_t)y * b->w;
        for (int x = 0; x < b->w; x++) {
            uint32_t r = (uint32_t)(x * 255 / b->w), g = (uint32_t)(y * 255 / b->h);
            row[x] = 0xff000000 | r << 16 | g << 8 | hue;
        }
    }
}

/* Menu : fond clair, cadre, trois lignes. */
static void draw_menu(struct buf *b) {
    for (int y = 0; y < b->h; y++)
        for (int x = 0; x < b->w; x++) {
            int edge = x == 0 || y == 0 || x == b->w - 1 || y == b->h - 1;
            int line = (y % 40) >= 16 && (y % 40) < 22 && x >= 12 && x < b->w - 12;
            b->px[(size_t)y * b->w + x] = edge ? 0xff303848 : line ? 0xff8090a8 : 0xfff2f4f8;
        }
}

/* Curseur 24×24 : croix sur fond transparent (alpha prémultiplié). */
static void draw_cursor(struct buf *b) {
    for (int y = 0; y < b->h; y++)
        for (int x = 0; x < b->w; x++) {
            int on = (abs(x - 4) <= 1 && y < 20) || (abs(y - 4) <= 1 && x < 20);
            b->px[(size_t)y * b->w + x] = on ? 0xffe0302c : 0;
        }
}

/* Valide un tampon et attend le rappel frame. */
static int show(struct app *a, struct buf *b) {
    wl_surface_attach(a->surf, b->wb, 0, 0);
    wl_surface_damage_buffer(a->surf, 0, 0, b->w, b->h);
    a->frame_done = 0;
    wl_callback_add_listener(wl_surface_frame(a->surf), &frame_listener, a);
    b->busy = 1;
    wl_surface_commit(a->surf);
    return WAIT(a->d, NULL, a->frame_done, 3000);
}

/* Fenêtre décorée par le serveur (décoration demandée avant le premier commit). */
static int open_window(struct app *a, const char *title) {
    a->surf = wl_compositor_create_surface(a->comp);
    a->xs = xdg_wm_base_get_xdg_surface(a->wm, a->surf);
    xdg_surface_add_listener(a->xs, &xs_listener, a);
    a->top = xdg_surface_get_toplevel(a->xs);
    xdg_toplevel_add_listener(a->top, &top_listener, a);
    xdg_toplevel_set_title(a->top, title);
    xdg_toplevel_set_app_id(a->top, "org.maios.wlinput");
    if (a->deco_mgr) {
        a->deco = zxdg_decoration_manager_v1_get_toplevel_decoration(a->deco_mgr, a->top);
        zxdg_toplevel_decoration_v1_add_listener(a->deco, &deco_listener, a);
        zxdg_toplevel_decoration_v1_set_mode(a->deco, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
    }
    wl_surface_commit(a->surf);
    return WAIT(a->d, NULL, a->configured, 3000);
}

/* Menu de `w`×`h` ancré au point (x, y) de la fenêtre ; saisie demandée si `grab`. */
static void open_popup(struct app *a, struct popup *p, int x, int y, int w, int h, int grab, uint32_t serial) {
    memset(p, 0, sizeof *p);
    struct xdg_positioner *pos = xdg_wm_base_create_positioner(a->wm);
    xdg_positioner_set_size(pos, w, h);
    xdg_positioner_set_anchor_rect(pos, x, y, 1, 1);
    xdg_positioner_set_anchor(pos, XDG_POSITIONER_ANCHOR_TOP_LEFT);
    xdg_positioner_set_gravity(pos, XDG_POSITIONER_GRAVITY_BOTTOM_RIGHT);
    p->surf = wl_compositor_create_surface(a->comp);
    p->xs = xdg_wm_base_get_xdg_surface(a->wm, p->surf);
    xdg_surface_add_listener(p->xs, &pxs_listener, p);
    p->p = xdg_surface_get_popup(p->xs, a->xs, pos);
    xdg_popup_add_listener(p->p, &popup_listener, p);
    xdg_positioner_destroy(pos);
    if (grab) xdg_popup_grab(p->p, a->seat, serial);
    wl_surface_commit(p->surf);
}

static void close_popup(struct popup *p) {
    xdg_popup_destroy(p->p);
    xdg_surface_destroy(p->xs);
    wl_surface_destroy(p->surf);
    memset(p, 0, sizeof *p);
}

/* Lit tout le tube `fd` (fermé ensuite) en servant les connexions : la source peut être l'une
 * d'elles. -1 : délai dépassé. */
static int read_pipe(struct wl_display *d1, struct wl_display *d2, int fd, char *out, size_t cap, int ms) {
    double end = now_ms() + ms;
    size_t n = 0;
    fcntl(fd, F_SETFL, O_NONBLOCK);
    for (;;) {
        ssize_t k = read(fd, out + n, cap - 1 - n);
        if (k > 0) {
            n += (size_t)k;
            if (n >= cap - 1) break;
            continue;
        }
        if (k == 0) break;
        if (errno != EAGAIN && errno != EINTR) break;
        if (step(d1, d2, fd, end) < 0 && now_ms() >= end) {
            close(fd);
            out[n] = 0;
            return -1;
        }
    }
    close(fd);
    out[n] = 0;
    return (int)n;
}

/* Colle la sélection courante de `a` (type `mime`) ; `other` est servie pendant la lecture. */
static int paste(struct app *a, struct wl_display *other, const char *mime, char *out, size_t cap) {
    int p[2];
    out[0] = 0;
    if (!a->sel || pipe2(p, O_CLOEXEC) < 0) return -1;
    wl_data_offer_receive(a->sel->o, mime, p[1]);
    wl_display_flush(a->d);
    close(p[1]);
    return read_pipe(a->d, other, p[0], out, cap, 3000);
}

static void teardown(struct app *a) {
    if (a->sel) {
        free(a->sel);
        a->sel = NULL;
    }
    if (a->d) wl_display_disconnect(a->d);
    if (a->state) xkb_state_unref(a->state);
    if (a->keymap) xkb_keymap_unref(a->keymap);
    if (a->xkb) xkb_context_unref(a->xkb);
    if (a->pool && a->pool != MAP_FAILED) munmap(a->pool, a->pool_size);
    if (a->fd >= 0) close(a->fd);
    memset(a, 0, sizeof *a);
    a->fd = -1;
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

/* --- étapes ------------------------------------------------------------------------------ */

static void test_keyboard(struct app *a) {
    say("pret clavier");
    int r = WAIT(a->d, NULL, strlen(a->text) >= 2 && a->down == 0, HARNESS_MS);
    const char *layout = a->keymap ? xkb_keymap_layout_get_name(a->keymap, 0) : NULL;
    int fr = layout && strstr(layout, "French");
    const char *want = fr ? "aQ" : "qA";
    CHECK(r == 0 && !strcmp(a->text, want), "touches q puis Maj+a : texte \"%s\" (attendu \"%s\", disposition %s)", a->text,
          want, layout ? layout : "-");
    int seq = 0;
    const uint32_t expect[3] = {KEY_Q, KEY_LEFTSHIFT, KEY_A};
    for (int i = 0; i < a->ncodes && seq < 3; i++)
        if (a->codes[i] == expect[seq]) seq++;
    CHECK(seq == 3, "codes evdev KEY_Q, KEY_LEFTSHIFT, KEY_A (%d touches recues)", a->ncodes);
    CHECK(a->shift_seen, "modificateurs : Maj (masque 1) signale pendant la touche");
}

static void test_pointer(struct app *a) {
    a->enters = a->motions = a->frames = a->axis_events = a->left_down = a->left_up = 0;
    a->axis120 = 0;
    say("pret souris");
    int r = WAIT(a->d, NULL, a->left_up && a->axis_events, HARNESS_MS);
    CHECK(r == 0, "souris : evenements recus (entrees %d, mouvements %d, trames %d)", a->enters, a->motions, a->frames);
    CHECK(a->ptr_surf == a->surf && a->enters >= 1, "wl_pointer.enter sur la fenetre (point %d,%d)", a->ix, a->iy);
    CHECK(a->left_down == 1 && a->left_up == 1, "BTN_LEFT enfonce puis relache");
    /* `mouse_move 0 0 1` de QEMU : un cran vers le haut ; Wayland : négatif vers le haut. */
    CHECK(a->axis120 == -120, "molette, un cran vers le haut : axis_value120 = %d (attendu -120)", a->axis120);
    CHECK(a->frames >= 3, "wl_pointer.frame apres chaque groupe (%d)", a->frames);
    CHECK(a->set_cursors >= 1 && !wl_display_get_error(a->d), "wl_pointer.set_cursor (surface 24x24) accepte");
    if (r) dump_trace();
}

static void test_bad_grab(struct app *a) {
    struct popup p;
    open_popup(a, &p, 20, 20, 80, 60, 1, a->btn_serial + 100000);
    int r = WAIT(a->d, NULL, p.done, 3000);
    CHECK(r == 0 && !wl_display_get_error(a->d), "menu saisissant a numero perime : popup_done, sans erreur");
    close_popup(&p);
    wl_display_roundtrip(a->d);
}

static void test_menu(struct app *a, struct buf *mb) {
    a->right_down = 0;
    a->shapes = 0;
    say("pret menu");
    int r = WAIT(a->d, NULL, a->right_down, HARNESS_MS);
    CHECK(r == 0, "bouton droit recu (numero %u)", a->btn_serial);
    if (r) {
        dump_trace();
        return;
    }
    struct popup p;
    int px = a->ix - 4, py = a->iy - 4;
    open_popup(a, &p, px, py, mb->w, mb->h, 1, a->btn_serial);
    r = WAIT(a->d, NULL, p.configured || p.done, 3000);
    CHECK(r == 0 && !p.done && p.x == px && p.y == py && p.w == mb->w && p.h == mb->h,
          "xdg_popup.configure %d,%d %dx%d (attendu %d,%d %dx%d)", p.x, p.y, p.w, p.h, px, py, mb->w, mb->h);
    xdg_surface_ack_configure(p.xs, p.serial);
    draw_menu(mb);
    wl_surface_attach(p.surf, mb->wb, 0, 0);
    wl_surface_damage_buffer(p.surf, 0, 0, mb->w, mb->h);
    wl_surface_commit(p.surf);
    wl_display_roundtrip(a->d);
    say("menu ouvert");
    r = WAIT(a->d, NULL, a->kbd_surf == p.surf, 3000);
    CHECK(r == 0, "saisie : le clavier passe au menu");
    r = WAIT(a->d, NULL, a->ptr_surf == p.surf && a->shapes, HARNESS_MS);
    CHECK(r == 0 && !p.done, "relachement : le menu reste ouvert, pointeur sur le menu");
    CHECK(a->shape_on == p.surf && !wl_display_get_error(a->d), "cursor-shape : forme \"pointer\" sur le menu");
    say("menu survole");
    r = WAIT(a->d, NULL, p.done, HARNESS_MS);
    CHECK(r == 0, "clic hors du menu : popup_done");
    r = WAIT(a->d, NULL, a->kbd_surf == a->surf, 3000);
    CHECK(r == 0, "clavier rendu a la fenetre");
    close_popup(&p);
    wl_display_roundtrip(a->d);
}

static void test_clipboard(struct app *A, struct app *B) {
    static const char TA[] = "MaiLand : texte copie par A, accents compris (é à ç)";
    static const char TB[] = "MaiLand : texte copie par B";
    static const char UTF8[] = "text/plain;charset=utf-8";
    char got[256];

    /* A copie, puis colle sa propre sélection. */
    A->src = wl_data_device_manager_create_data_source(A->ddm);
    wl_data_source_add_listener(A->src, &src_listener, A);
    wl_data_source_offer(A->src, UTF8);
    wl_data_source_offer(A->src, "text/plain");
    A->src_text = TA;
    int before = A->sel_count;
    wl_data_device_set_selection(A->dd, A->src, A->key_serial);
    int r = WAIT(A->d, NULL, A->sel_count > before && A->sel && A->src_sends >= 1, 3000);
    CHECK(r == 0 && has_mime(A->sel, UTF8) && has_mime(A->sel, "text/plain"),
          "A : selection posee, texte lu aussitot par le serveur, offre presentee");
    r = paste(A, NULL, UTF8, got, sizeof got);
    CHECK(r >= 0 && !strcmp(got, TA), "A colle sa propre selection (%d octets)", r);

    /* B ouvre une fenêtre : il prend le focus et reçoit l'offre ; il colle le texte de A. */
    struct buf bb;
    if (connect_app(B, "B") || make_pool(B, 320 * 200 * 4)) {
        CHECK(0, "connexion du second client");
        return;
    }
    get_input(B);
    r = open_window(B, "wlinput B - MaiLand");
    xdg_surface_ack_configure(B->xs, B->serial);
    make_buf(B, &bb, 0, 320, 200, WL_SHM_FORMAT_XRGB8888);
    draw(&bb, 0xc0);
    CHECK(r == 0 && show(B, &bb) == 0, "B : fenetre affichee");
    r = WAIT(B->d, A->d, B->kbd_surf == B->surf && B->sel && A->kbd_surf == NULL, 3000);
    CHECK(r == 0, "B prend le clavier (A le perd) et recoit l'offre de A");
    r = paste(B, A->d, UTF8, got, sizeof got);
    CHECK(r >= 0 && !strcmp(got, TA), "B colle le texte de A (%d octets, la source de A sert)", r);

    /* B copie : la source de A est annulée. */
    B->src = wl_data_device_manager_create_data_source(B->ddm);
    wl_data_source_add_listener(B->src, &src_listener, B);
    wl_data_source_offer(B->src, UTF8);
    B->src_text = TB;
    before = B->sel_count;
    wl_data_device_set_selection(B->dd, B->src, B->key_serial);
    r = WAIT(B->d, A->d, A->src_cancelled && B->src_sends >= 1 && B->sel_count > before, 3000);
    CHECK(r == 0, "B copie : wl_data_source.cancelled chez A");
    wl_data_source_destroy(A->src);
    A->src = NULL;

    /* B détruit sa source : le texte reste collable (copie du serveur). */
    wl_display_roundtrip(B->d);
    wl_data_source_destroy(B->src);
    B->src = NULL;
    before = B->sel_count;
    r = WAIT(B->d, A->d, B->sel_count > before && B->sel, 3000);
    CHECK(r == 0 && has_mime(B->sel, UTF8) && has_mime(B->sel, "UTF8_STRING"), "source detruite : nouvelle offre du serveur");
    r = paste(B, A->d, UTF8, got, sizeof got);
    CHECK(r >= 0 && !strcmp(got, TB), "le texte survit a sa source (%d octets)", r);
    r = paste(B, A->d, "UTF8_STRING", got, sizeof got);
    CHECK(r >= 0 && !strcmp(got, TB), "meme texte en UTF8_STRING");
}

static void test_errors(void) {
    struct app e;
    if (connect_app(&e, "E") == 0) {
        wl_seat_get_touch(e.seat);
        expect_error(&e, "wl_seat.get_touch sans ecran tactile", "wl_seat", WL_SEAT_ERROR_MISSING_CAPABILITY);
    } else {
        CHECK(0, "connexion pour get_touch");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0 && e.shape_mgr) {
        struct wl_pointer *p = wl_seat_get_pointer(e.seat);
        struct wp_cursor_shape_device_v1 *dev = wp_cursor_shape_manager_v1_get_pointer(e.shape_mgr, p);
        wp_cursor_shape_device_v1_set_shape(dev, 0, 0);
        expect_error(&e, "forme de curseur 0", "wp_cursor_shape_device_v1", WP_CURSOR_SHAPE_DEVICE_V1_ERROR_INVALID_SHAPE);
    } else {
        CHECK(0, "connexion pour set_shape");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0) {
        struct xdg_positioner *pos = xdg_wm_base_create_positioner(e.wm);
        xdg_positioner_set_size(pos, 0, 10);
        expect_error(&e, "positionneur de largeur 0", "xdg_positioner", XDG_POSITIONER_ERROR_INVALID_INPUT);
    } else {
        CHECK(0, "connexion pour le positionneur");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0 && e.deco_mgr) {
        struct wl_surface *s = wl_compositor_create_surface(e.comp);
        struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(e.wm, s);
        struct xdg_toplevel *t = xdg_surface_get_toplevel(xs);
        zxdg_decoration_manager_v1_get_toplevel_decoration(e.deco_mgr, t);
        zxdg_decoration_manager_v1_get_toplevel_decoration(e.deco_mgr, t);
        expect_error(&e, "seconde decoration", "zxdg_decoration_manager_v1",
                     ZXDG_TOPLEVEL_DECORATION_V1_ERROR_ALREADY_CONSTRUCTED);
    } else {
        CHECK(0, "connexion pour la decoration");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0 && e.deco_mgr) {
        struct wl_surface *s = wl_compositor_create_surface(e.comp);
        struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(e.wm, s);
        struct xdg_toplevel *t = xdg_surface_get_toplevel(xs);
        struct zxdg_toplevel_decoration_v1 *dd = zxdg_decoration_manager_v1_get_toplevel_decoration(e.deco_mgr, t);
        zxdg_toplevel_decoration_v1_set_mode(dd, 7);
        expect_error(&e, "mode de decoration 7", "zxdg_toplevel_decoration_v1", ZXDG_TOPLEVEL_DECORATION_V1_ERROR_INVALID_MODE);
    } else {
        CHECK(0, "connexion pour le mode de decoration");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0 && e.ddm) {
        struct wl_data_device *dd = wl_data_device_manager_get_data_device(e.ddm, e.seat);
        struct wl_data_source *src = wl_data_device_manager_create_data_source(e.ddm);
        wl_data_device_set_selection(dd, src, 0);
        wl_data_device_set_selection(dd, src, 0);
        expect_error(&e, "source donnee deux fois", "wl_data_device", WL_DATA_DEVICE_ERROR_USED_SOURCE);
    } else {
        CHECK(0, "connexion pour le presse-papiers");
    }
    teardown(&e);

    if (connect_app(&e, "E") == 0 && make_pool(&e, 64 * 64 * 4) == 0 && open_window(&e, "erreur") == 0) {
        struct popup p;
        xdg_surface_ack_configure(e.xs, e.serial);
        open_popup(&e, &p, 0, 0, 32, 32, 0, 0);
        wl_display_roundtrip(e.d);
        xdg_popup_grab(p.p, e.seat, 0);
        expect_error(&e, "saisie apres le premier commit", "xdg_popup", XDG_POPUP_ERROR_INVALID_GRAB);
    } else {
        CHECK(0, "connexion pour la saisie tardive");
    }
    teardown(&e);
}

int main(void) {
    struct app A, B;
    printf("wlinput : debut (pid %d)\n", getpid());
    fflush(stdout);

    /* 1. Globales et siège. */
    int r = connect_app(&A, "A");
    CHECK(r == 0 && A.ddm && A.deco_mgr && A.shape_mgr,
          "globales wl_seat, wl_data_device_manager, zxdg_decoration_manager_v1, wp_cursor_shape_manager_v1");
    if (r || !A.ddm || !A.deco_mgr || !A.shape_mgr || A.out_w <= 0) {
        printf("wlinput : FIN %d ok, %d echec\n", n_ok, n_ko + 1);
        return 1;
    }
    CHECK(A.seat_caps == (WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD) && !strcmp(A.seat_name, "seat0"),
          "wl_seat \"%s\" : capacites %u (pointeur + clavier)", A.seat_name, A.seat_caps);
    get_input(&A);
    /* Comme GTK, Qt ou SDL : contexte par défaut, il faut les règles de xkb-data. */
    CHECK(A.xkb != NULL, "xkb_context_new : regles XKB installees (/usr/share/X11/xkb)");
    wl_display_roundtrip(A.d);
    CHECK(A.keymap_fmt == WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 && A.keymap && A.state, "carte XKB compilee par libxkbcommon (%s)",
          A.keymap ? xkb_keymap_layout_get_name(A.keymap, 0) : "-");
    CHECK(A.keymap_ro, "carte projetee en ecriture partagee : refuse (lecture seule)");
    CHECK(A.rate == 25 && A.delay == 600, "repeat_info %d/s apres %d ms", A.rate, A.delay);

    /* 2. Fenêtre décorée par le serveur, puis agrandie. */
    const int W = 480, H = 320, MW = 160, MH = 120;
    size_t big = (size_t)A.out_w * A.out_h * 4, win = (size_t)W * H * 4, menu = (size_t)MW * MH * 4;
    r = make_pool(&A, win + big + menu + 24 * 24 * 4);
    CHECK(r == 0, "pool memfd de %zu octets", A.pool_size);
    if (r) {
        printf("wlinput : FIN %d ok, %d echec\n", n_ok, n_ko);
        return 1;
    }
    r = open_window(&A, "wlinput - MaiLand");
    CHECK(r == 0 && A.deco_configs >= 1 && A.deco_mode == ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE,
          "decoration : mode server_side annonce avant la configuration initiale");
    xdg_surface_ack_configure(A.xs, A.serial);
    struct buf bw, bbig, bmenu, bcur;
    make_buf(&A, &bw, 0, W, H, WL_SHM_FORMAT_XRGB8888);
    make_buf(&A, &bmenu, win + big, MW, MH, WL_SHM_FORMAT_XRGB8888);
    make_buf(&A, &bcur, win + big + menu, 24, 24, WL_SHM_FORMAT_ARGB8888);
    draw(&bw, 0x60);
    r = show(&A, &bw);
    CHECK(r == 0, "fenetre %dx%d affichee", W, H);
    r = WAIT(A.d, NULL, A.kbd_surf == A.surf && A.activated, 3000);
    CHECK(r == 0, "nouvelle fenetre : wl_keyboard.enter et etat activated");

    int cfgs = A.configured;
    xdg_toplevel_set_maximized(A.top);
    r = WAIT(A.d, NULL, A.configured > cfgs && A.maximized, 3000);
    CHECK(r == 0 && A.cfg_w > W && A.cfg_h > H && A.cfg_w <= A.out_w && A.cfg_h <= A.out_h && A.activated,
          "agrandie : configure %dx%d (ecran %dx%d), etats maximized + activated", A.cfg_w, A.cfg_h, A.out_w, A.out_h);
    if (r == 0) {
        xdg_surface_ack_configure(A.xs, A.serial);
        make_buf(&A, &bbig, win, A.cfg_w, A.cfg_h, WL_SHM_FORMAT_XRGB8888);
        draw(&bbig, 0x60);
        CHECK(show(&A, &bbig) == 0, "contenu agrandi affiche");
    }

    /* 3. Clavier. */
    test_keyboard(&A);

    /* 4. Pointeur, curseur du client. */
    A.cursor_surf = wl_compositor_create_surface(A.comp);
    draw_cursor(&bcur);
    wl_surface_attach(A.cursor_surf, bcur.wb, 0, 0);
    wl_surface_damage_buffer(A.cursor_surf, 0, 0, 24, 24);
    wl_surface_commit(A.cursor_surf);
    test_pointer(&A);

    /* 5. Menus. */
    test_bad_grab(&A);
    test_menu(&A, &bmenu);
    CHECK(A.pings >= 1, "xdg_wm_base.ping recus et rendus (%d)", A.pings);

    /* 6. Presse-papiers entre deux clients. */
    memset(&B, 0, sizeof B);
    B.fd = -1;
    test_clipboard(&A, &B);
    CHECK(!wl_display_get_error(A.d) && (!B.d || !wl_display_get_error(B.d)), "aucune erreur de protocole (A et B)");
    teardown(&B);
    teardown(&A);

    /* 7. Erreurs attendues. */
    test_errors();
    r = connect_app(&A, "A");
    CHECK(r == 0, "le serveur repond toujours apres les erreurs");
    teardown(&A);

    printf("wlinput : FIN %d ok, %d echec\n", n_ok, n_ko);
    fflush(stdout);
    return n_ko ? 1 : 0;
}
