use rquickjs::{Ctx, Function, Result as JsResult};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

use crate::events::Event;

/// Install the `sendEvent(name, payloadJson)` global and the low-level
/// `__sol_send_event` alias used by the runtime bridge.
pub(crate) const MAX_SEND_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;

pub(crate) fn install<'js>(
    ctx: Ctx<'js>,
    tx: UnboundedSender<Event>,
    last_error: Arc<Mutex<Option<String>>>,
) -> JsResult<()> {
    let globals = ctx.globals();

    let sender = tx.clone();
    let last_error = Arc::clone(&last_error);
    let send_fn = Function::new(
        ctx.clone(),
        move |name: String, payload_json: String| -> JsResult<()> {
            if let Ok(mut error_state) = last_error.lock() {
                *error_state = None;
            }

            if payload_json.len() > MAX_SEND_EVENT_PAYLOAD_BYTES {
                if let Ok(mut error_state) = last_error.lock() {
                    *error_state = Some(format!(
                        "sendEvent payload too large: {} bytes (max {})",
                        payload_json.len(),
                        MAX_SEND_EVENT_PAYLOAD_BYTES
                    ));
                }
                return Ok(());
            }

            let payload: serde_json::Value =
                serde_json::from_str(&payload_json).unwrap_or_else(|err| {
                    if let Ok(mut error_state) = last_error.lock() {
                        *error_state = Some(err.to_string());
                    }
                    serde_json::Value::Null
                });
            if let Err(_err) = sender.send(Event { name, payload }) {
                if let Ok(mut err_state) = last_error.lock() {
                    *err_state = Some("sendEvent receiver was dropped".into());
                }
            }
            Ok(())
        },
    )?;

    globals.set("__sol_send_event", send_fn.clone())?;
    globals.set("sendEvent", send_fn)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Runtime};
    use tokio::sync::mpsc;

    #[test]
    fn send_event_reaches_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let last_error = Arc::new(Mutex::new(Some("stale".into())));
        ctx.with(|ctx| {
            install(ctx.clone(), tx, Arc::clone(&last_error)).unwrap();
            let _: rquickjs::Value = ctx
                .eval(r#"sendEvent("click", JSON.stringify({x: 1}))"#)
                .unwrap();
        });
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "click");
        assert_eq!(ev.payload, serde_json::json!({"x":1}));
        assert_eq!(*last_error.lock().unwrap(), None);
    }

    #[test]
    fn send_event_records_parse_error() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let last_error = Arc::new(Mutex::new(None));
        ctx.with(|ctx| {
            let (_tx, mut _rx) = mpsc::unbounded_channel::<Event>();
            install(ctx.clone(), _tx, Arc::clone(&last_error)).unwrap();
            let _: rquickjs::Value = ctx.eval(r#"sendEvent("invalid", "{invalid")"#).unwrap();
        });
        let err = last_error.lock().unwrap().clone();
        assert!(err.is_some_and(|msg| !msg.is_empty()));
    }

    #[test]
    fn send_event_rejects_oversized_payload() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let last_error = Arc::new(Mutex::new(None));
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let payload = " ".repeat(MAX_SEND_EVENT_PAYLOAD_BYTES + 1);
        let code = format!(r#"sendEvent("oversize", "{}")"#, payload);
        ctx.with(|ctx| {
            install(ctx.clone(), tx, Arc::clone(&last_error)).unwrap();
            let _: rquickjs::Value = ctx.eval(code.as_str()).unwrap();
        });
        assert!(rx.try_recv().is_err());
        let err = last_error.lock().unwrap().clone();
        assert!(
            err.as_ref()
                .map(|msg| msg.contains("payload too large"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn send_event_records_send_failure_when_receiver_is_dropped() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let last_error = Arc::new(Mutex::new(None));
        drop(rx);
        ctx.with(|ctx| {
            install(ctx.clone(), tx, Arc::clone(&last_error)).unwrap();
            let _: rquickjs::Value = ctx.eval(r#"sendEvent("click", "{}")"#).unwrap();
        });
        assert_eq!(
            last_error.lock().unwrap().clone().as_deref(),
            Some("sendEvent receiver was dropped")
        );
    }
}
