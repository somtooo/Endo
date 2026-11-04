/*
 * Minimal client for the virtual-keyboard and wlr-virtual-pointer protocols.
 * Protocol definitions: Copyright 2008-2011 Kristian Høgsberg,
 * 2010-2013 Intel Corporation, 2012-2013 Collabora, Ltd., 2018 Purism SPC,
 * and 2019 Josef Gajdusek.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>

static const struct wl_interface keyboard_interface;
static const struct wl_interface pointer_interface;
static const struct wl_interface *keyboard_types[] = {&wl_seat_interface, &keyboard_interface};
static const struct wl_message keyboard_requests[] = {{"keymap", "uhu", NULL}};
static const struct wl_interface keyboard_interface = {
    "zwp_virtual_keyboard_v1", 1, 1, keyboard_requests, 0, NULL
};
static const struct wl_message keyboard_manager_requests[] = {
    {"create_virtual_keyboard", "on", keyboard_types}
};
static const struct wl_interface keyboard_manager_interface = {
    "zwp_virtual_keyboard_manager_v1", 1, 1, keyboard_manager_requests, 0, NULL
};
static const struct wl_interface *pointer_types[] = {&wl_seat_interface, &pointer_interface};
static const struct wl_interface pointer_interface = {
    "zwlr_virtual_pointer_v1", 1, 0, NULL, 0, NULL
};
static const struct wl_message pointer_manager_requests[] = {
    {"create_virtual_pointer", "?on", pointer_types}
};
static const struct wl_interface pointer_manager_interface = {
    "zwlr_virtual_pointer_manager_v1", 1, 1, pointer_manager_requests, 0, NULL
};

static struct wl_seat *seat;
static struct wl_proxy *keyboard_manager;
static struct wl_proxy *pointer_manager;

static void on_global(void *data, struct wl_registry *registry, uint32_t name,
                      const char *interface, uint32_t version) {
    (void)data;
    if (strcmp(interface, "wl_seat") == 0 && !seat)
        seat = wl_registry_bind(registry, name, &wl_seat_interface, version < 7 ? version : 7);
    else if (strcmp(interface, "zwp_virtual_keyboard_manager_v1") == 0 && !keyboard_manager)
        keyboard_manager = wl_registry_bind(registry, name, &keyboard_manager_interface, 1);
    else if (strcmp(interface, "zwlr_virtual_pointer_manager_v1") == 0 && !pointer_manager)
        pointer_manager = wl_registry_bind(registry, name, &pointer_manager_interface, 1);
}

static void on_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {on_global, on_remove};

int main(void) {
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) {
        fprintf(stderr, "cannot connect to private Wayland display\n");
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    if (!registry || wl_registry_add_listener(registry, &registry_listener, NULL) < 0 ||
        wl_display_roundtrip(display) < 0 || !seat || !keyboard_manager || !pointer_manager) {
        fprintf(stderr, "missing required virtual-input globals\n");
        return 2;
    }
    struct wl_proxy *keyboard = wl_proxy_marshal_flags(keyboard_manager, 0, &keyboard_interface, 1, 0, seat, NULL);
    struct wl_proxy *pointer = wl_proxy_marshal_flags(pointer_manager, 0, &pointer_interface, 1, 0, seat, NULL);
    struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_rule_names rules = {.rules = "evdev", .model = "pc105", .layout = "us"};
    struct xkb_keymap *keymap = context ? xkb_keymap_new_from_names(context, &rules, XKB_KEYMAP_COMPILE_NO_FLAGS) : NULL;
    char *text = keymap ? xkb_keymap_get_as_string(keymap, XKB_KEYMAP_FORMAT_TEXT_V1) : NULL;
    if (!keyboard || !pointer || !text) {
        fprintf(stderr, "virtual device or keymap creation failed\n");
        return 3;
    }
    size_t size = strlen(text) + 1;
    if (size > UINT32_MAX) {
        fprintf(stderr, "keymap too large\n");
        return 4;
    }
    int fd = memfd_create("cua-test-keymap", MFD_CLOEXEC);
    if (fd < 0) {
        perror("memfd_create");
        return 4;
    }
    for (size_t written = 0; written < size;) {
        ssize_t count = write(fd, text + written, size - written);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0) {
            perror("keymap write");
            return 4;
        }
        written += (size_t)count;
    }
    wl_proxy_marshal_flags(keyboard, 0, NULL, 1, 0, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, (uint32_t)size);
    close(fd);
    free(text);
    xkb_keymap_unref(keymap);
    xkb_context_unref(context);
    if (wl_display_roundtrip(display) < 0)
        return 5;
    puts("virtual pointer and keyboard ready");
    fflush(stdout);
    while (wl_display_dispatch(display) >= 0) {}
    return 0;
}
