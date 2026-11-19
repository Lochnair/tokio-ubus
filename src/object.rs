// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::blobmsg::{ArgType, Table, Value};
use crate::error::Status;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What a method handler returns: reply data (or none), or a failure status.
pub type Reply = Result<Option<Table>, Status>;

type BoxFuture = Pin<Box<dyn Future<Output = Reply> + Send + 'static>>;
pub(crate) type Handler = Arc<dyn Fn(Request) -> BoxFuture + Send + Sync + 'static>;

/// A call to one of an object's methods.
#[derive(Debug, Clone)]
pub struct Request {
    pub object: Arc<str>,
    pub method: Arc<str>,
    pub args: Table,
    /// The caller's ubusd ACL identity, when ubusd passes one along.
    pub user: Option<String>,
    pub group: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Method {
    pub name: Arc<str>,
    pub policy: Vec<(String, ArgType)>,
    pub handler: Handler,
}

/// A ubus object: a path such as `unravel` and its methods.
#[derive(Clone)]
pub struct Object {
    pub(crate) name: Arc<str>,
    pub(crate) methods: Vec<Method>,
}

impl Object {
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            methods: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Adds a method without advertised arguments.
    pub fn method<F, Fut>(self, name: impl Into<Arc<str>>, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        self.method_with_args(name, [] as [(String, ArgType); 0], handler)
    }

    /// Adds a method, advertising its arguments in the object's signature.
    /// The handler gets whatever the caller sent; checking it is up to the
    /// handler, as with libubus.
    pub fn method_with_args<F, Fut, S>(
        mut self,
        name: impl Into<Arc<str>>,
        args: impl IntoIterator<Item = (S, ArgType)>,
        handler: F,
    ) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
        S: Into<String>,
    {
        let name = name.into();
        assert!(
            self.methods.iter().all(|m| m.name != name),
            "method {name:?} added twice to {:?}",
            self.name
        );
        self.methods.push(Method {
            name,
            policy: args.into_iter().map(|(n, t)| (n.into(), t)).collect(),
            handler: Arc::new(move |req| Box::pin(handler(req))),
        });
        self
    }

    pub(crate) fn find(&self, method: &[u8]) -> Option<&Method> {
        self.methods.iter().find(|m| m.name.as_bytes() == method)
    }

    /// The `UBUS_ATTR_SIGNATURE` contents: a table per method, mapping each
    /// argument name to its type.
    pub(crate) fn signature(&self) -> Table {
        self.methods
            .iter()
            .map(|m| {
                let args: Table = m
                    .policy
                    .iter()
                    .map(|(n, t)| (n.clone(), Value::Int32(*t as i32)))
                    .collect();
                (m.name.to_string(), args)
            })
            .collect()
    }
}

impl std::fmt::Debug for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Object")
            .field("name", &self.name)
            .field(
                "methods",
                &self.methods.iter().map(|m| &*m.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}
