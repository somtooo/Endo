use std::collections::HashMap;

use gpui::{App, Global, SharedString, SystemNotification};

/// Whether "agent is done" notifications go to the operating system rather
/// than a popup window. Wayland has no primary display, and doesn't let a
/// window place itself in a corner of the screen.
pub(crate) fn preferred(cx: &App) -> bool {
    cfg!(any(target_os = "linux", target_os = "freebsd")) && cx.compositor_name() == "Wayland"
}

#[derive(Default)]
struct NotificationRouter {
    responses: HashMap<SharedString, Box<dyn FnOnce(&mut App)>>,
}

impl Global for NotificationRouter {}

pub(crate) fn show(
    cx: &mut App,
    tag: SharedString,
    title: SharedString,
    body: SharedString,
    on_response: impl FnOnce(&mut App) + 'static,
) {
    if !cx.has_global::<NotificationRouter>() {
        cx.set_global(NotificationRouter::default());
        cx.on_system_notification_response(|response, cx| {
            let handler = cx
                .global_mut::<NotificationRouter>()
                .responses
                .remove(&response.tag);
            if let Some(handler) = handler {
                handler(cx);
            }
        });
    }
    cx.global_mut::<NotificationRouter>()
        .responses
        .insert(tag.clone(), Box::new(on_response));
    cx.show_system_notification(SystemNotification {
        tag,
        title,
        body,
        actions: Vec::new(),
    });
}

/// Linux can't retract a notification, so it may stay in the shade after this.
/// Its handler stays too, so that clicking it still opens the thread.
pub(crate) fn dismiss(cx: &mut App, tag: &str) {
    cx.dismiss_system_notification(tag);
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{SystemNotificationResponse, TestAppContext};

    use super::*;

    #[gpui::test]
    async fn responses_are_routed_by_tag_and_outlive_dismissal(cx: &mut TestAppContext) {
        let responses = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            cx.set_app_identity("dev.zed.Zed", "Zed");
            for tag in ["thread", "terminal", "dismissed"] {
                let responses = responses.clone();
                show(cx, tag.into(), tag.into(), "".into(), move |_| {
                    responses.borrow_mut().push(tag);
                });
            }
            dismiss(cx, "dismissed");
        });

        for tag in ["terminal", "dismissed", "thread", "terminal", "unknown"] {
            cx.simulate_system_notification_response(SystemNotificationResponse {
                tag: tag.into(),
                action_id: None,
            });
        }
        assert_eq!(*responses.borrow(), ["terminal", "dismissed", "thread"]);
    }
}
