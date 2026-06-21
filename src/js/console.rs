use rquickjs::{Ctx, Result as JsResult};
#[cfg(debug_assertions)]
use rquickjs::Function;

/// Install `__sol_dev_warn(message)` — a stderr diagnostic channel the runtime
/// uses to flag likely developer mistakes (e.g. a function passed to a reactive
/// attribute instead of being called, which silently makes it non-reactive).
///
/// The binding is only installed in debug builds, so release bundles stay
/// silent and the runtime's optional-chained `__sol_dev_warn?.(...)` call skips
/// without even building the message.
pub(crate) fn install(ctx: Ctx<'_>) -> JsResult<()> {
    #[cfg(debug_assertions)]
    {
        let warn = Function::new(ctx.clone(), |message: String| -> JsResult<()> {
            eprintln!("[solite] {message}");
            Ok(())
        })?;
        ctx.globals().set("__sol_dev_warn", warn)?;
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = ctx;
    }
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use rquickjs::{Context, Runtime};

    #[test]
    fn dev_warn_is_callable_in_debug() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            super::install(ctx.clone()).unwrap();
            let is_fn: bool = ctx
                .eval("typeof __sol_dev_warn === 'function'")
                .unwrap();
            assert!(is_fn, "__sol_dev_warn should be installed in debug builds");
            // Calling it must not throw.
            let _: rquickjs::Value = ctx.eval("__sol_dev_warn('hello from test')").unwrap();
        });
    }
}
