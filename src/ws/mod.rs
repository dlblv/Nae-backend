use json::JsonValue;

mod start;
pub(crate) use start::start_connection;

mod messages;
pub(crate) use messages::*;

pub(crate) mod engine_io;
pub(crate) mod socket_io;

fn error<S: AsRef<str>>(class_name: S, name: S, code: u16, message: S) -> JsonValue {
  json::object! {
    className: class_name.as_ref(),
    code: code,
    message: message.as_ref(),
    name: name.as_ref(),
  }
}

pub(crate) fn error_not_found<S: AsRef<str>>(message: S) -> JsonValue {
  error("not-found", "NotFound", 404, message.as_ref())
}

pub(crate) fn error_general<S: AsRef<str>>(message: S) -> JsonValue {
  error("general-errors", "GeneralError", 500, message.as_ref())
}

