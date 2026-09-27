//! Desktop notifications (XDG portal on Linux).

/// Shows a notification; failures are only logged.
pub fn show(rt: &tokio::runtime::Handle, id: &str, title: String, body: String) {
    #[cfg(target_os = "linux")]
    {
        let id = id.to_owned();
        rt.spawn(async move {
            use ashpd::desktop::notification::{Notification, NotificationProxy, Priority};
            let result = async {
                let proxy = NotificationProxy::new().await?;
                let notification = Notification::new(&title)
                    .body(body.as_str())
                    .icon(ashpd::desktop::Icon::with_names(["io.github.cheviiot.okno"]))
                    .priority(Priority::High);
                proxy.add_notification(&id, notification).await
            }
            .await;
            if let Err(e) = result {
                tracing::debug!("notification failed: {e}");
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rt, id, title, body);
    }
}
