#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>

struct wl_display;
struct wlr_xdg_shell;

struct wlr_xdg_shell *wlr_xdg_shell_create(struct wl_display *display, uint32_t version) {
    typedef struct wlr_xdg_shell *(*create_fn)(struct wl_display *, uint32_t);
    create_fn create = (create_fn)dlsym(RTLD_NEXT, "wlr_xdg_shell_create");
    if (!create)
        abort();
    return create(display, version == 5 ? 6 : version);
}
