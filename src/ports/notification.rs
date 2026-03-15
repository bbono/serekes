/// Port for sending messages/notifications to the user.
#[allow(dead_code)]
pub trait NotificationPort: Send + Sync {
    fn send(&self, msg: String);
}
