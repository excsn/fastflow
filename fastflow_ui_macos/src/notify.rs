use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_user_notifications::{
  UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
  UNUserNotificationCenter,
};

use crate::applog::log;

/// Asks once. macOS remembers the answer and later calls return it without a prompt.
pub fn request_permission() {
  let center = UNUserNotificationCenter::currentNotificationCenter();
  let done = RcBlock::new(|granted: Bool, _err: *mut NSError| {
    log(format!("notifications allowed: {}", granted.as_bool()));
  });
  center.requestAuthorizationWithOptions_completionHandler(
    UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
    &done,
  );
}

pub fn post(id: &str, title: &str, body: &str) {
  let content = UNMutableNotificationContent::new();
  content.setTitle(&NSString::from_str(title));
  content.setBody(&NSString::from_str(body));
  let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
    &NSString::from_str(id),
    &content,
    None,
  );
  UNUserNotificationCenter::currentNotificationCenter()
    .addNotificationRequest_withCompletionHandler(&request, None);
}
