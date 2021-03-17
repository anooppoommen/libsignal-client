//
// Copyright 2021 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use super::*;

use paste::paste;
use std::fmt;

const ERRORS_PROPERTY_NAME: &str = "Errors";

#[allow(non_snake_case)]
fn node_registerErrorClasses(mut cx: FunctionContext) -> JsResult<JsValue> {
    let errors_module = cx.argument::<JsObject>(0)?;
    cx.this()
        .set(&mut cx, ERRORS_PROPERTY_NAME, errors_module)?;
    Ok(cx.undefined().upcast())
}
node_register!(registerErrorClasses);

fn new_js_error<'a>(
    cx: &mut impl Context<'a>,
    module: Handle<'a, JsObject>,
    name: &str,
    args: impl IntoIterator<Item = Handle<'a, JsValue>>,
) -> Option<Handle<'a, JsObject>> {
    let result = cx.try_catch(|cx| {
        let errors_module: Handle<JsObject> = module
            .get(cx, ERRORS_PROPERTY_NAME)?
            .downcast_or_throw(cx)?;
        let error_class: Handle<JsFunction> = errors_module.get(cx, name)?.downcast_or_throw(cx)?;
        error_class.construct(cx, args)
    });
    match result {
        Ok(error_instance) => Some(error_instance),
        Err(failure) => {
            log::warn!(
                "could not construct {}: {}",
                name,
                failure
                    .to_string(cx)
                    .map(|s| s.value(cx))
                    .unwrap_or_else(|_| "(could not print error)".to_owned())
            );
            None
        }
    }
}

pub trait SignalNodeError {
    fn throw<'a>(
        self,
        cx: &mut impl Context<'a>,
        module: Handle<'a, JsObject>,
    ) -> JsResult<'a, JsValue>;
}

impl SignalNodeError for neon::result::Throw {
    fn throw<'a>(
        self,
        _cx: &mut impl Context<'a>,
        _module: Handle<'a, JsObject>,
    ) -> JsResult<'a, JsValue> {
        Err(self)
    }
}

impl SignalNodeError for SignalProtocolError {
    fn throw<'a>(
        self,
        cx: &mut impl Context<'a>,
        module: Handle<'a, JsObject>,
    ) -> JsResult<'a, JsValue> {
        // Check for some dedicated error types first.
        match &self {
            SignalProtocolError::UntrustedIdentity(addr) => {
                let addr_string = cx.string(addr.name());
                if let Some(error) = new_js_error(
                    cx,
                    module,
                    "UntrustedIdentityError",
                    vec![addr_string.upcast()],
                ) {
                    return cx.throw(error);
                }
            }
            SignalProtocolError::SealedSenderSelfSend => {
                let message = cx.string(self.to_string());
                if let Some(error) =
                    new_js_error(cx, module, "SealedSenderSelfSend", vec![message.upcast()])
                {
                    return cx.throw(error);
                }
            }
            _ => {}
        }
        cx.throw_error(self.to_string())
    }
}

impl SignalNodeError for device_transfer::Error {
    fn throw<'a>(
        self,
        cx: &mut impl Context<'a>,
        _module: Handle<'a, JsObject>,
    ) -> JsResult<'a, JsValue> {
        cx.throw_error(self.to_string())
    }
}

impl SignalNodeError for signal_crypto::Error {
    fn throw<'a>(
        self,
        cx: &mut impl Context<'a>,
        _module: Handle<'a, JsObject>,
    ) -> JsResult<'a, JsValue> {
        cx.throw_error(self.to_string())
    }
}

/// Represents an error returned by a callback.
#[derive(Debug)]
struct CallbackError {
    message: String,
}

impl CallbackError {
    fn new(message: String) -> CallbackError {
        Self { message }
    }
}

impl fmt::Display for CallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "callback error {}", self.message)
    }
}

impl std::error::Error for CallbackError {}

/// Converts a JavaScript error message to a [`SignalProtocolError::ApplicationCallbackError`].
pub fn js_error_to_rust(func: &'static str, err: String) -> SignalProtocolError {
    SignalProtocolError::ApplicationCallbackError(func, Box::new(CallbackError::new(err)))
}
