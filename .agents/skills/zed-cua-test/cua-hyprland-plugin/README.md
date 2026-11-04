# Patched Cua Hyprland plugin (vendored)

This is the Cua Hyprland plugin that `zedtest` builds and loads into its
private Hyprland. It is the build source only (`src`, `include`, `cmake`,
`CMakeLists.txt`), without upstream's tests and docs. License: MIT
(`LICENSE.md`, Cua AI, Inc.).

It is the same source as Omarchy's `cua-hyprland-plugin` package, plus one fix:

1. Cua at commit `c82d32e3e1adbc6578a148962002ddf6e3e8a15a` (release 0.33.2),
   `libs/cua-driver/hyprland-plugin`.
2. `patches/omarchy-downstream.patch`: Omarchy's `downstream.patch` from
   https://github.com/omacom/omarchy-pkgs/tree/master/pkgbuilds/cua-hyprland-plugin
   (SHA-256 `87d3f560cfbd195748321a7e7ad2cb921c77cc022877de7890d3a711940f6e3c`).
   The harness needs the `ime_popup_guard` status it adds.
3. `patches/wheel-detents.patch`: sends the wheel-click count with each scroll.
   Without it Zed ignores scrolls, because it ignores a wheel event that has
   no `axis_value120` or `axis_discrete`. Proposed upstream in
   https://github.com/trycua/cua/pull/6489.

It was qualified with Hyprland 0.56.2. To update it after a Hyprland or Cua
release, take the commit named in Omarchy's current `PKGBUILD`, apply its
`downstream.patch` and `wheel-detents.patch` (drop the latter once Cua ships
the fix), and copy the four build paths here. Then run `zedtest setup` and a
short test that clicks, types and scrolls.
