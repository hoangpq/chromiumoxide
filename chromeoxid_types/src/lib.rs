use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

pub mod target;
pub use target::*;
pub mod browser;
pub use browser::*;

pub mod dom;
pub use dom::*;

pub mod runtime;
pub use runtime::*;

pub mod page;

pub use page::*;
use std::ops::Deref;

/// A Message sent by the client
#[derive(Serialize, Debug, PartialEq)]
pub struct MethodCall {
    /// Identifier for this method call
    ///
    /// [`MethodCall`] id's must be unique for every session
    pub id: CallId,
    pub method: Cow<'static, str>,
    /// Json byte vector
    pub params: serde_json::Value,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallId(usize);

impl CallId {
    pub fn new(id: usize) -> Self {
        CallId(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

pub trait Command: serde::ser::Serialize + Method {
    type Response: serde::de::DeserializeOwned + fmt::Debug;

    fn create_call(&self, call_id: CallId) -> serde_json::Result<MethodCall> {
        Ok(MethodCall {
            id: call_id,
            method: self.method_name(),
            params: serde_json::to_value(self)?,
        })
    }

    fn to_vec(&self, call_id: CallId) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&self.create_call(call_id)?)
    }
}

pub struct CommandResponse<T>
where
    T: fmt::Debug,
{
    pub id: CallId,
    pub result: T,
    pub method: Cow<'static, str>,
}

pub type CommandResult<T> = Result<CommandResponse<T>, Error>;

impl<T: fmt::Debug> Deref for CommandResponse<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

/// An event produced by the Chrome instance
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct CdpEvent {
    /// Name of the method
    pub method: Cow<'static, str>,
    /// Json params
    pub params: serde_json::Value,
}

impl Method for CdpEvent {
    fn identifier(&self) -> Cow<'static, str> {
        self.method.clone()
    }
}

impl Event for CdpEvent {
    fn session_id(&self) -> Option<&str> {
        self.params.get("sessionId").and_then(|x| x.as_str())
    }
}

pub trait Event: Method + DeserializeOwned {
    fn session_id(&self) -> Option<&str>;
}

pub trait Method {
    /// The whole string identifier for this method like: `DOM.removeNode`
    fn identifier(&self) -> Cow<'static, str>;

    /// The name of the domain this method belongs to: `DOM`
    fn domain_name(&self) -> Cow<'static, str> {
        self.split().0
    }

    /// The standalone identifier of the method inside the domain: `removeNode`
    fn method_name(&self) -> Cow<'static, str> {
        self.split().1
    }

    /// Tuple of (`domain_name`, `method_name`) : (`DOM`, `removeNode`)
    fn split(&self) -> (Cow<'static, str>, Cow<'static, str>) {
        match self.identifier() {
            Cow::Borrowed(id) => {
                let mut iter = id.split('.');
                (iter.next().unwrap().into(), iter.next().unwrap().into())
            }
            Cow::Owned(id) => {
                let mut iter = id.split('.');
                (
                    Cow::Owned(iter.next().unwrap().into()),
                    Cow::Owned(iter.next().unwrap().into()),
                )
            }
        }
    }
}

/// A response to a [`MethodCall`] from the Chrome instance
#[derive(Deserialize, Debug, PartialEq, Clone)]
pub struct Response {
    /// matching [`MethodCall`] identifier
    pub id: CallId,
    /// The response payload
    pub result: Option<serde_json::Value>,
    /// The Reason why the [`MethodCall`] failed.
    pub error: Option<Error>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum Message<T = CdpEvent> {
    Event(T),
    Response(Response),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    /// Error code
    pub code: usize,
    /// Error Message
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}
