use rquickjs::{Ctx, Function, Result as JsResult};
use tokio::sync::mpsc::UnboundedSender;

use crate::events::Event;

/// Install the `sendEvent(name, payloadJson)` global and the low-level
/// `__ox_send_event` alias used by the runtime bridge.
pub(crate) fn install<'js>(ctx: Ctx<'js>, tx: UnboundedSender<Event>) -> JsResult<()> {
    let globals = ctx.globals();

    let sender = tx.clone();
    let send_fn = Function::new(
        ctx.clone(),
        move |name: String, payload_json: String| -> JsResult<()> {
            let payload: serde_json::Value =
                serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
            let _ = sender.send(Event { name, payload });
            Ok(())
        },
    )?;

    globals.set("__ox_send_event", send_fn.clone())?;
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
        ctx.with(|ctx| {
            install(ctx.clone(), tx).unwrap();
            let _: rquickjs::Value = ctx
                .eval(r#"sendEvent("click", JSON.stringify({x: 1}))"#)
                .unwrap();
        });
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "click");
    }
}
