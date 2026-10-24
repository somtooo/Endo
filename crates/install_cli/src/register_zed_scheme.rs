use client::ZED_URL_SCHEME;
use gpui::{AsyncApp, actions};

actions!(
    cli,
    [
        /// Registers the zed:// URL scheme handler.
        RegisterZedScheme
    ]
);

pub async fn register_zed_scheme(cx: &AsyncApp) -> anyhow::Result<()> {
    cx.update(|cx| {
        let scheme = if release_channel::ReleaseChannel::global(cx)
            == release_channel::ReleaseChannel::Endo
        {
            "endo"
        } else {
            ZED_URL_SCHEME
        };
        cx.register_url_scheme(scheme)
    })
    .await
}
