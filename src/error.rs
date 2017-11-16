use futures::sync::mpsc;
use js::{jsapi, jsval};
use js::conversions::{ConversionResult, FromJSValConvertible};
use std::ffi;
use std::fmt;
use std::io;
use std::ptr;

/// The kind of error that occurred.
#[derive(Debug, ErrorChain)]
pub enum ErrorKind {
    /// Some other kind of miscellaneous error, described in the given string.
    Msg(String),

    /// An IO error.
    #[error_chain(foreign)]
    Io(io::Error),

    /// Tried to send a value on a channel when the receiving half was already
    /// dropped.
    #[error_chain(foreign)]
    SendError(mpsc::SendError<()>),

    /// Could not create a JavaScript runtime.
    #[error_chain(custom)]
    #[error_chain(description = r#"|| "Could not create a JavaScript Runtime""#)]
    #[error_chain(display = r#"|| write!(f, "Could not create a JavaScript Runtime")"#)]
    CouldNotCreateJavaScriptRuntime,

    /// Could not read a value from a channel.
    #[error_chain(custom)]
    #[error_chain(description = r#"|| "Could not read a value from a channel""#)]
    #[error_chain(display = r#"|| write!(f, "Could not read a value from a channel")"#)]
    CouldNotReadValueFromChannel,

    /// There was an exception in JavaScript code.
    #[error_chain(custom)]
    #[error_chain(description = r#"|_| "JavaScript exception""#)]
    #[error_chain(display = r#"|e| write!(f, "{}", e)"#)]
    JavaScriptException(JsException),

    /// There was an unhandled, rejected JavaScript promise.
    // TODO: stack, line, column, filename, etc
    #[error_chain(custom)]
    #[error_chain(description = r#"|| "Unhandled, rejected JavaScript promise""#)]
    #[error_chain(display = r#"|| write!(f, "Unhandled, rejected JavaScript promise")"#)]
    JavaScriptUnhandledRejectedPromise,

    /// The JavaScript `Promise` that was going to settle this future was
    /// reclaimed by the garbage collector without having been resolved or
    /// rejected.
    #[error_chain(custom)]
    #[error_chain(description = r#"|| "JavaScript Promise collected without settling""#)]
    #[error_chain(display = r#"|| write!(f, "JavaScript Promise collected without settling")"#)]
    JavaScriptPromiseCollectedWithoutSettling,
}

impl Clone for Error {
    fn clone(&self) -> Self {
        self.to_string().into()
    }
}

/// A trait for structured error types that can be constructed from a pending
/// JSAPI exception.
///
// TODO: Should this move this into mozjs?
pub trait FromPendingJsapiException: fmt::Debug + FromJSValConvertible<Config=()> {
    /// Construct `Self` from the given JS value.
    ///
    /// If the `FromJSValConvertible` implementation for `Self` can fail, then
    /// override this default implementation so that it never fails.
    unsafe fn infallible_from_jsval(
        cx: *mut jsapi::JSContext,
        val: jsapi::JS::HandleValue,
    ) -> Self {
        match Self::from_jsval(cx, val, ()) {
            Ok(ConversionResult::Success(v)) => v,
            otherwise => panic!("infallible_from_jsval: {:?}", otherwise),
        }
    }

    /// Given a `cx` if it has a pending expection, take it and constuct a
    /// `Self`. Otherwise, return `None`.
    unsafe fn take_pending(cx: *mut jsapi::JSContext) -> Option<Self> {
        if jsapi::JS_IsExceptionPending(cx) {
            rooted!(in(cx) let mut val = jsval::UndefinedValue());
            assert!(jsapi::JS_GetPendingException(cx, val.handle_mut()));
            jsapi::JS_ClearPendingException(cx);
            Some(Self::infallible_from_jsval(cx, val.handle()))
        } else {
            None
        }
    }
}

type CResult<T> = ::std::result::Result<ConversionResult<T>, ()>;

impl FromJSValConvertible for Error {
    type Config = ();

    #[inline]
    unsafe fn from_jsval(
        cx: *mut jsapi::JSContext,
        val: jsapi::JS::HandleValue,
        _: ()
    ) -> CResult<Error> {
        Ok(ErrorKind::from_jsval(cx, val, ())?.map(|ek| ek.into()))
    }
}

impl FromPendingJsapiException for Error {}

impl FromJSValConvertible for ErrorKind {
    type Config = ();

    #[inline]
    unsafe fn from_jsval(
        cx: *mut jsapi::JSContext,
        val: jsapi::JS::HandleValue,
        _: ()
    ) -> CResult<ErrorKind> {
        Ok(JsException::from_jsval(cx, val, ())?.map(ErrorKind::JavaScriptException))
    }
}

impl FromPendingJsapiException for ErrorKind {}

/// An exception that was thrown in JavaScript, or a promise was rejected with.
#[derive(Debug, Clone)]
pub enum JsException {
    /// The value thrown or rejected was not an `Error` object, so we
    /// stringified it into this value.
    Stringified(String),

    /// The value thrown or rejected was an `Error` object.
    Error {
        /// The error message.
        message: String,
        /// The JavaScript filename, if any.
        filename: Option<String>,
        /// The line number the error originated on.
        line: u32,
        /// The column number the error originated on.
        column: u32,
        /// The JavaScript stack when the error was created, if any.
        stack: Option<String>,
    },
}

impl fmt::Display for JsException {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            JsException::Stringified(ref s) => write!(f, "{}", s),
            JsException::Error { ref message, ref filename, line, column, ref stack } => {
                if let Some(ref filename) = *filename {
                    write!(f, "{}:", filename)?;
                }

                write!(f, "{}:{}: {}", line, column, message)?;

                if let Some(ref stack) = *stack {
                    write!(f, "\n\nStack:\n{}", stack)?;
                }

                Ok(())
            }
        }
    }
}

impl FromJSValConvertible for JsException {
    type Config = ();

    unsafe fn from_jsval(
        cx: *mut jsapi::JSContext,
        val: jsapi::JS::HandleValue,
        _: ()
    ) -> CResult<JsException> {
        // First try and convert the value into a JSErrorReport (aka some kind
        // of `Error` or `TypeError` etc.) If this fails, we'll just stringify
        // the value and use that as the error.
        rooted!(in(cx) let mut obj = ptr::null_mut());
        let report = if val.is_object() {
            obj.set(val.to_object());
            jsapi::JS_ErrorFromException(cx, obj.handle())
        } else {
            ptr::null_mut()
        };
        if report.is_null() {
            let stringified = match String::from_jsval(cx, val, ()) {
                Ok(ConversionResult::Success(s)) => s,
                Ok(ConversionResult::Failure(why)) => {
                    format!("<could not convert error value to string: {}>", why)
                }
                Err(()) => "<could not convert error value to string>".into()
            };
            debug_assert!(!jsapi::JS_IsExceptionPending(cx));
            return Ok(ConversionResult::Success(JsException::Stringified(stringified)));
        }

        // Ok, we have an error report. Pull out all the metadata we can get
        // from it: filename, line, column, etc.

        let filename = (*report)._base.filename;
        let filename = if !filename.is_null() {
            Some(ffi::CStr::from_ptr(filename).to_string_lossy().to_string())
        } else {
            None
        };

        let line = (*report)._base.lineno;
        let column = (*report)._base.column;

        let message = (*report)._base.message_.data_;
        let message = ffi::CStr::from_ptr(message).to_string_lossy().to_string();

        debug_assert!(!obj.is_null());
        rooted!(in(cx) let stack = jsapi::ExceptionStackOrNull(obj.handle()));
        let stack = if stack.is_null() {
            None
        } else {
            rooted!(in(cx) let mut stack_string = ptr::null_mut());
            assert!(jsapi::JS::BuildStackString(
                cx,
                stack.handle(),
                stack_string.handle_mut(),
                0,
                jsapi::js::StackFormat::Default,
            ));
            rooted!(in(cx) let stack_string_val = jsval::StringValue(
                stack_string.get().as_ref().unwrap()
            ));
            match String::from_jsval(cx, stack_string_val.handle(), ()) {
                Ok(ConversionResult::Success(s)) => Some(s),
                _ => None,
            }
        };

        debug_assert!(!jsapi::JS_IsExceptionPending(cx));
        Ok(ConversionResult::Success(JsException::Error {
            message,
            filename,
            line,
            column,
            stack
        }))
    }
}

impl FromPendingJsapiException for JsException {}
