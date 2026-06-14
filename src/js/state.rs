use rquickjs::{Ctx, Function, Result as JsResult};

use crate::state::StateHandle;

/// Install `__ox_state_set` and `__ox_apply_state_patch` globals.
///
/// - `__ox_state_set(path, value_json)` – called by the JS store when the user
///   mutates state directly in JS; mirrors the value into Rust without re-queuing.
/// - `__ox_apply_state_patch` is called by `tick()` (Rust side) at JS eval time;
///   it is not registered as a global—the Rust code evaluates the call directly.
pub(crate) fn install<'js>(ctx: Ctx<'js>, state: &StateHandle) -> JsResult<()> {
    let globals = ctx.globals();

    let state_mirror = state.clone();
    globals.set(
        "__ox_state_set",
        Function::new(
            ctx.clone(),
            move |path: String, value_json: String| -> JsResult<()> {
                let v: serde_json::Value =
                    serde_json::from_str(&value_json).unwrap_or(serde_json::Value::Null);
                state_mirror.mirror_js_write(&path, v);
                Ok(())
            },
        ),
    )?;

    // Initialise the JS store with the current Rust snapshot.
    let snapshot = serde_json::to_string(&state.snapshot()).unwrap_or_else(|_| "{}".into());
    let init_code = format!(
        r#"
        if (typeof __ox_state !== 'undefined' && typeof __ox_state.__init === 'function') {{
            __ox_state.__init({snapshot});
        }}
        "#,
    );
    // Best-effort: if the state object isn't available yet (M1), this is a no-op.
    let _: rquickjs::Value = ctx
        .eval(init_code)
        .unwrap_or(rquickjs::Value::new_null(ctx.clone()));

    Ok(())
}

/// Emit a JS call to apply a Rust-side state patch.
///
/// Evaluates `__ox_apply_state_patch(path, json_value)` in the given context.
pub(crate) fn apply_patch<'js>(
    ctx: Ctx<'js>,
    path: &str,
    value: &serde_json::Value,
) -> JsResult<()> {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    let code = format!(
        r#"if (typeof __ox_apply_state_patch === 'function') {{
            __ox_apply_state_patch({path:?}, {json});
        }}"#,
    );
    let _: rquickjs::Value = ctx
        .eval(code)
        .unwrap_or(rquickjs::Value::new_null(ctx.clone()));
    Ok(())
}
