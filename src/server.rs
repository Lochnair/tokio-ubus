// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use crate::blobmsg::Table;
use crate::error::{Error, Status};
use crate::object::{Object, Request};
use crate::wire::{Builder, Message, MsgType, Reader, attr};
use log::{error, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::{self, JoinError, JoinSet};

/// Where ubusd listens on OpenWrt.
pub const DEFAULT_SOCKET: &str = "/var/run/ubus/ubus.sock";

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_MAX_CALLS: usize = 64;

/// Publishes a set of objects on ubus and answers calls to them.
///
/// Each call runs as its own task, so a slow handler does not hold up the
/// others. Replies may arrive out of order; ubusd matches them to callers.
pub struct Server {
    socket: PathBuf,
    objects: Vec<Object>,
    registered: watch::Sender<bool>,
    max_calls: usize,
}

impl Server {
    /// A server for the ubusd socket at [`DEFAULT_SOCKET`].
    pub fn new() -> Self {
        Self::with_socket(DEFAULT_SOCKET)
    }

    pub fn with_socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            objects: Vec::new(),
            registered: watch::channel(false).0,
            max_calls: DEFAULT_MAX_CALLS,
        }
    }

    /// How many calls may run at once; 64 by default. Calls past the limit
    /// are refused with [`Status::NO_MEMORY`] rather than queued, so a flood
    /// of calls to slow handlers cannot grow memory without bound.
    pub fn max_calls(mut self, max_calls: usize) -> Self {
        assert!(max_calls > 0, "max_calls must be at least 1");
        self.max_calls = max_calls;
        self
    }

    /// Adds an object. Panics if one with the same name was already added.
    pub fn object(mut self, object: Object) -> Self {
        assert!(
            self.objects.iter().all(|o| o.name != object.name),
            "object {:?} added twice",
            object.name
        );
        self.objects.push(object);
        self
    }

    /// Whether every object is currently registered with ubusd. Meant for a
    /// server with one connection at a time, as [`Server::run`] keeps.
    ///
    /// An object that fails to register, say because another client already
    /// holds its name, keeps this false while the others are served. Every
    /// object is tried again after a reconnect, including ones that failed;
    /// libubus retries only those that registered.
    pub fn registered(&self) -> watch::Receiver<bool> {
        self.registered.subscribe()
    }

    /// Connects to ubusd and serves calls, reconnecting and registering the
    /// objects again whenever the connection is lost (ubusd restarting, for
    /// example). Never returns; drop the future to stop. Each failure is
    /// logged, with retries backing off up to 30 seconds.
    pub async fn run(&self) -> ! {
        let mut backoff = MIN_BACKOFF;
        loop {
            let (error, was_registered) = match UnixStream::connect(&self.socket).await {
                Ok(stream) => self.session(stream).await,
                Err(error) => (Error::Io(error), false),
            };
            // A session that got as far as registering resets the backoff.
            if was_registered {
                backoff = MIN_BACKOFF;
            }
            warn!(
                "ubus connection to {} lost: {error}; retrying in {}s",
                self.socket.display(),
                backoff.as_secs()
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Serves one connection to ubusd until it fails, returning the reason.
    /// In-flight calls are cancelled when it does, or when the future is
    /// dropped.
    pub async fn serve<S>(&self, stream: S) -> Error
    where
        S: AsyncRead + AsyncWrite + Send,
    {
        self.session(stream).await.0
    }

    /// One session, and whether it had registered every object.
    async fn session<S>(&self, stream: S) -> (Error, bool)
    where
        S: AsyncRead + AsyncWrite + Send,
    {
        let mut session = Session::new(self);
        let error = session.run(stream).await;
        (error, session.unregister())
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

struct Session<'a> {
    server: &'a Server,
    /// The ADD_OBJECT awaiting its reply. Objects register one at a time,
    /// as with libubus, so the broker never has more than one to answer.
    pending: Option<Pending>,
    /// The next object to register.
    next: usize,
    /// Registered objects by the id ubusd assigned.
    by_id: HashMap<u32, usize>,
    /// Whether ubusd refused any object this session.
    refused: bool,
    /// Running handlers, each producing its encoded reply. Dropping the set
    /// with the session aborts them.
    calls: JoinSet<Option<Vec<u8>>>,
    /// Who to answer when a handler panics instead.
    callers: HashMap<task::Id, Call>,
    /// Whether this session set the server's registered flag, and so is the
    /// one to clear it.
    registered: bool,
}

impl Drop for Session<'_> {
    /// Cancelling a registered session must not leave observers thinking it
    /// still is.
    fn drop(&mut self) {
        self.unregister();
    }
}

struct Pending {
    seq: u16,
    object: usize,
    /// From the DATA that precedes the STATUS.
    id: Option<u32>,
}

struct Call {
    seq: u16,
    peer: u32,
    objid: u32,
    no_reply: bool,
    object: Arc<str>,
    method: Arc<str>,
}

impl<'a> Session<'a> {
    fn new(server: &'a Server) -> Self {
        Self {
            server,
            pending: None,
            next: 0,
            by_id: HashMap::new(),
            refused: false,
            calls: JoinSet::new(),
            callers: HashMap::new(),
            registered: false,
        }
    }

    /// Clears the server's registered flag if this session set it,
    /// returning whether it did.
    fn unregister(&mut self) -> bool {
        let was = std::mem::take(&mut self.registered);
        if was {
            self.server.registered.send_replace(false);
        }
        was
    }

    async fn run<S>(&mut self, stream: S) -> Error
    where
        S: AsyncRead + AsyncWrite + Send,
    {
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = Reader::new(rd);
        let mut wr = Box::pin(wr);
        let hello = match reader.next().await {
            Ok(hello) => hello,
            Err(error) => return error,
        };
        if hello.ty != Some(MsgType::Hello) {
            return Error::Protocol("expected HELLO from ubusd");
        }
        if let Err(error) = self.register_next(&mut wr).await {
            return error;
        }
        loop {
            let result = tokio::select! {
                msg = reader.next() => match msg {
                    Ok(msg) => self.handle(msg, &mut wr).await,
                    Err(error) => Err(error),
                },
                Some(done) = self.calls.join_next_with_id() => self.finish_call(done, &mut wr).await,
            };
            if let Err(error) = result {
                return error;
            }
        }
    }

    /// Sends the next ADD_OBJECT, or reports the outcome once every object
    /// has been tried.
    async fn register_next<W>(&mut self, wr: &mut W) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        while let Some(object) = self.server.objects.get(self.next) {
            let index = self.next;
            self.next += 1;
            // Sequence numbers only need to tell our own requests apart.
            let seq = index as u16 + 1;
            match add_object(object, seq) {
                Some(bytes) => {
                    self.pending = Some(Pending {
                        seq,
                        object: index,
                        id: None,
                    });
                    return Ok(wr.write_all(&bytes).await?);
                }
                // Like ubus_add_object, fail this object and go on.
                None => {
                    error!(
                        "Registering ubus object {:?} failed: its signature exceeds the message limit",
                        object.name
                    );
                    self.refused = true;
                }
            }
        }
        let mut names: Vec<_> = self
            .by_id
            .values()
            .map(|&i| self.server.objects[i].name())
            .collect();
        names.sort_unstable();
        if !names.is_empty() {
            info!("Registered ubus objects: {}", names.join(", "));
        }
        if !self.refused {
            self.server.registered.send_replace(true);
            self.registered = true;
        }
        Ok(())
    }

    async fn handle<W>(&mut self, msg: Message, wr: &mut W) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        match msg.ty {
            // ubusd answers ADD_OBJECT as peer 0. Any client owning an object
            // can send DATA or STATUS to us too, arriving with its object's
            // id as the peer, so like libubus match the peer as well as the
            // sequence number.
            Some(MsgType::Data | MsgType::Status)
                if msg.peer == 0 && self.pending.as_ref().is_some_and(|p| p.seq == msg.seq) =>
            {
                self.registration_reply(&msg, wr).await
            }
            Some(MsgType::Invoke) => self.invoke(&msg, wr).await,
            // Subscription changes and anything newer are not ours to handle.
            _ => Ok(()),
        }
    }

    async fn registration_reply<W>(&mut self, msg: &Message, wr: &mut W) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        let attrs = msg.attrs()?;
        let pending = self.pending.as_mut().expect("checked by caller");
        if let Some(id) = attrs.objid {
            pending.id = Some(id);
        }
        if msg.ty != Some(MsgType::Status) {
            return Ok(());
        }
        let pending = self.pending.take().expect("checked by caller");
        let object = &self.server.objects[pending.object];
        match (attrs.status, pending.id) {
            (Some(0), Some(id)) => {
                self.by_id.insert(id, pending.object);
            }
            (Some(0), None) => {
                return Err(Error::Protocol("ADD_OBJECT succeeded without an object id"));
            }
            // Like libubus, a refused object does not stop the others.
            (status, _) => {
                let status = Status(status.unwrap_or(Status::UNKNOWN_ERROR.0));
                error!("Registering ubus object {:?} failed: {status}", object.name);
                self.refused = true;
            }
        }
        self.register_next(wr).await
    }

    async fn invoke<W>(&mut self, msg: &Message, wr: &mut W) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        let attrs = msg.attrs()?;
        // libubus drops an INVOKE without an object id unanswered.
        let Some(objid) = attrs.objid else {
            return Ok(());
        };
        let (seq, peer, no_reply) = (msg.seq, msg.peer, attrs.no_reply);
        let refuse = |status| status_reply(seq, peer, objid, status);

        let Some(&index) = self.by_id.get(&objid) else {
            return send_unless(no_reply, wr, refuse(Status::NOT_FOUND)).await;
        };
        let object = &self.server.objects[index];
        let Some(method_name) = attrs.method else {
            return send_unless(no_reply, wr, refuse(Status::INVALID_ARGUMENT)).await;
        };
        let Some(method) = object.find(method_name) else {
            return send_unless(no_reply, wr, refuse(Status::METHOD_NOT_FOUND)).await;
        };
        // Calls that finished but were not joined yet still hold a slot.
        while self.calls.len() >= self.server.max_calls {
            let Some(done) = self.calls.try_join_next_with_id() else {
                break;
            };
            self.finish_call(done, wr).await?;
        }
        if self.calls.len() >= self.server.max_calls {
            warn!(
                "ubus call to {}.{} refused: {} calls already running",
                object.name,
                method.name,
                self.calls.len()
            );
            return send_unless(no_reply, wr, refuse(Status::NO_MEMORY)).await;
        }
        let Some(args) = attrs.data.and_then(|data| Table::decode(data).ok()) else {
            return send_unless(no_reply, wr, refuse(Status::INVALID_ARGUMENT)).await;
        };

        let request = Request {
            object: Arc::clone(&object.name),
            method: Arc::clone(&method.name),
            args,
            user: attrs.user.map(str::to_owned),
            group: attrs.group.map(str::to_owned),
        };
        let handler = Arc::clone(&method.handler);
        // Calling the handler inside the task means a panic while creating
        // its future is caught along with one while polling it.
        let task = self.calls.spawn(async move {
            let result = handler(request).await;
            (!no_reply).then(|| match result {
                Ok(Some(table)) => data_reply(seq, peer, objid, &table),
                Ok(None) => status_reply(seq, peer, objid, Status::OK),
                Err(status) => status_reply(seq, peer, objid, status),
            })
        });
        self.callers.insert(
            task.id(),
            Call {
                seq,
                peer,
                objid,
                no_reply,
                object: Arc::clone(&object.name),
                method: Arc::clone(&method.name),
            },
        );
        Ok(())
    }

    async fn finish_call<W>(
        &mut self,
        done: Result<(task::Id, Option<Vec<u8>>), JoinError>,
        wr: &mut W,
    ) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        let reply = match done {
            Ok((id, reply)) => {
                self.callers.remove(&id);
                reply
            }
            // Only a panic gets here: nothing aborts a call while the
            // session runs.
            Err(error) => {
                let call = self
                    .callers
                    .remove(&error.id())
                    .expect("every call is recorded");
                error!("ubus handler for {}.{} panicked", call.object, call.method);
                (!call.no_reply)
                    .then(|| status_reply(call.seq, call.peer, call.objid, Status::UNKNOWN_ERROR))
            }
        };
        let Some(reply) = reply else {
            return Ok(());
        };
        wr.write_all(&reply).await?;
        Ok(())
    }
}

async fn send_unless<W>(no_reply: bool, wr: &mut W, bytes: Vec<u8>) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    if !no_reply {
        wr.write_all(&bytes).await?;
    }
    Ok(())
}

/// An object's ADD_OBJECT, or `None` if its signature does not fit.
fn add_object(object: &Object, seq: u16) -> Option<Vec<u8>> {
    let mut msg = Builder::new(MsgType::AddObject, seq, 0);
    let w = msg.writer();
    w.put_str(attr::OBJPATH, &object.name);
    let signature = w.start(attr::SIGNATURE, false);
    object.signature().encode_contents(w);
    w.end(signature);
    msg.finish()
}

/// The STATUS that ends a call, as `ubus_complete_deferred_request` sends it.
fn status_reply(seq: u16, peer: u32, objid: u32, status: Status) -> Vec<u8> {
    let mut msg = Builder::new(MsgType::Status, seq, peer);
    msg.writer().put_u32(attr::STATUS, status.0 as u32);
    msg.writer().put_u32(attr::OBJID, objid);
    msg.finish().expect("a status message is small")
}

/// DATA followed by a successful STATUS, or only a failed STATUS when the
/// data does not fit in a message.
fn data_reply(seq: u16, peer: u32, objid: u32, table: &Table) -> Vec<u8> {
    let mut msg = Builder::new(MsgType::Data, seq, peer);
    let w = msg.writer();
    w.put_u32(attr::OBJID, objid);
    let data = w.start(attr::DATA, false);
    table.encode_contents(w);
    w.end(data);
    match msg.finish() {
        Some(mut out) => {
            out.extend(status_reply(seq, peer, objid, Status::OK));
            out
        }
        None => {
            error!("ubus reply exceeds the message limit; answering with an error");
            status_reply(seq, peer, objid, Status::UNKNOWN_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobmsg::{ArgType, Value};
    use crate::object::Reply;
    use crate::wire::Message;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    /// The broker end of a connection, driven by the test.
    struct Broker {
        reader: Reader<ReadHalf<DuplexStream>>,
        wr: WriteHalf<DuplexStream>,
    }

    impl Broker {
        async fn send(
            &mut self,
            ty: MsgType,
            seq: u16,
            peer: u32,
            build: impl FnOnce(&mut crate::blob::Writer),
        ) {
            let mut msg = Builder::new(ty, seq, peer);
            build(msg.writer());
            self.wr.write_all(&msg.finish().unwrap()).await.unwrap();
        }

        async fn recv(&mut self) -> Message {
            tokio::time::timeout(Duration::from_secs(2), self.reader.next())
                .await
                .expect("no message from the server")
                .unwrap()
        }

        /// HELLO, then accept every ADD_OBJECT, returning (path, signature).
        async fn accept(&mut self, objects: usize) -> Vec<(String, Table)> {
            self.send(MsgType::Hello, 0, 0x1234, |_| {}).await;
            let mut out = Vec::new();
            for i in 0..objects {
                let msg = self.recv().await;
                assert_eq!(msg.ty, Some(MsgType::AddObject));
                let mut path = None;
                let mut signature = None;
                for a in crate::blob::iter(&msg.body) {
                    let a = a.unwrap();
                    match a.id {
                        attr::OBJPATH => {
                            path = Some(
                                std::str::from_utf8(&a.data[..a.data.len() - 1])
                                    .unwrap()
                                    .to_owned(),
                            )
                        }
                        attr::SIGNATURE => signature = Some(Table::decode(a.data).unwrap()),
                        _ => {}
                    }
                }
                let id = 100 + i as u32;
                self.send(MsgType::Data, msg.seq, 0, |w| {
                    w.put_u32(attr::OBJID, id);
                    w.put_u32(attr::OBJTYPE, 200 + i as u32);
                })
                .await;
                self.send(MsgType::Status, msg.seq, 0, |w| w.put_u32(attr::STATUS, 0))
                    .await;
                out.push((path.unwrap(), signature.unwrap()));
            }
            out
        }

        async fn invoke(
            &mut self,
            seq: u16,
            objid: u32,
            method: &str,
            args: &Table,
            no_reply: bool,
        ) {
            self.send(MsgType::Invoke, seq, 0x99, |w| {
                w.put_u32(attr::OBJID, objid);
                w.put_str(attr::METHOD, method);
                let d = w.start(attr::DATA, false);
                args.encode_contents(w);
                w.end(d);
                if no_reply {
                    w.put_u8(attr::NO_REPLY, 1);
                }
                w.put_str(attr::USER, "root");
                w.put_str(attr::GROUP, "root");
            })
            .await;
        }

        /// Reads a call's reply: its data, if any, and status.
        async fn reply(&mut self) -> (u16, Option<Table>, i32) {
            let mut data = None;
            loop {
                let msg = self.recv().await;
                assert_eq!(msg.peer, 0x99, "replies go to the caller");
                let attrs = msg.attrs().unwrap();
                match msg.ty {
                    Some(MsgType::Data) => {
                        assert_eq!(attrs.objid, Some(100));
                        data = Some(Table::decode(attrs.data.unwrap()).unwrap());
                    }
                    Some(MsgType::Status) => {
                        assert!(attrs.objid.is_some());
                        return (msg.seq, data, attrs.status.unwrap());
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    fn start(server: Server) -> (Broker, Arc<Server>, tokio::task::JoinHandle<Error>) {
        let (ours, theirs) = tokio::io::duplex(4 * 1024 * 1024);
        let server = Arc::new(server);
        let task = tokio::spawn({
            let server = Arc::clone(&server);
            async move { server.serve(ours).await }
        });
        let (rd, wr) = tokio::io::split(theirs);
        (
            Broker {
                reader: Reader::new(rd),
                wr,
            },
            server,
            task,
        )
    }

    fn example() -> Server {
        Server::new()
            .object(
                Object::new("test")
                    .method("echo", |req| async move {
                        let mut t = req.args.clone();
                        t.push("user", req.user.clone());
                        t.push("method", &*req.method);
                        Ok(Some(t))
                    })
                    .method_with_args("slow", [("ms", ArgType::Int32)], |req| async move {
                        let ms = req.args.get("ms").and_then(Value::as_i64).unwrap_or(0);
                        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                        Ok(None)
                    })
                    .method("fail", |_| async { Err(Status::PERMISSION_DENIED) })
                    .method("panic", |_| async { panic!("boom") })
                    .method("setup_panic", |_| -> std::future::Ready<Reply> {
                        panic!("before the future exists")
                    })
                    .method("huge", |_| async {
                        Ok(Some(Table::new().with("x", "a".repeat(2 * 1024 * 1024))))
                    }),
            )
            .object(Object::new("test.other").method("noop", |_| async { Ok(None) }))
    }

    #[tokio::test]
    async fn registers_objects_with_signatures() {
        let (mut broker, server, _task) = start(example());
        let mut registered = server.registered();
        let objects = broker.accept(2).await;
        assert_eq!(objects[0].0, "test");
        assert_eq!(objects[1].0, "test.other");
        let sig = &objects[0].1;
        let names: Vec<_> = sig.iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            ["echo", "slow", "fail", "panic", "setup_panic", "huge"]
        );
        assert_eq!(
            sig.get("slow").and_then(Value::as_table),
            Some(&Table::new().with("ms", ArgType::Int32 as i32))
        );
        assert_eq!(
            sig.get("echo").and_then(Value::as_table),
            Some(&Table::new())
        );
        tokio::time::timeout(Duration::from_secs(1), registered.wait_for(|r| *r))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn answers_calls() {
        let (mut broker, _server, _task) = start(example());
        broker.accept(2).await;
        let args = Table::new()
            .with("a", 1i32)
            .with("nested", Table::new().with("b", "c"));
        broker.invoke(7, 100, "echo", &args, false).await;
        let (seq, data, status) = broker.reply().await;
        assert_eq!((seq, status), (7, 0));
        let expected = args.clone().with("user", "root").with("method", "echo");
        assert_eq!(data, Some(expected));

        broker.invoke(8, 100, "slow", &Table::new(), false).await;
        assert_eq!(broker.reply().await, (8, None, 0));
    }

    #[tokio::test]
    async fn reports_failures_as_status() {
        let (mut broker, _server, task) = start(example());
        broker.accept(2).await;
        for (seq, objid, method, status) in [
            (1, 100, "missing", Status::METHOD_NOT_FOUND),
            (2, 555, "echo", Status::NOT_FOUND),
            (3, 100, "fail", Status::PERMISSION_DENIED),
            (4, 100, "panic", Status::UNKNOWN_ERROR),
            (5, 100, "setup_panic", Status::UNKNOWN_ERROR),
            (6, 100, "huge", Status::UNKNOWN_ERROR),
        ] {
            broker
                .invoke(seq, objid, method, &Table::new(), false)
                .await;
            assert_eq!(broker.reply().await, (seq, None, status.0), "{method}");
        }
        assert!(!task.is_finished(), "failed calls keep the connection up");
    }

    #[tokio::test]
    async fn forged_registration_replies_are_ignored() {
        let (mut broker, _server, _task) = start(example());
        broker.send(MsgType::Hello, 0, 0x1234, |_| {}).await;
        let add = broker.recv().await;
        assert_eq!((add.ty, add.seq), (Some(MsgType::AddObject), 1));
        // Another object's owner, relayed by ubusd with its object id as
        // the peer, claims the registration under an id of its choosing.
        let forger = 0xdead;
        broker
            .send(MsgType::Data, add.seq, forger, |w| {
                w.put_u32(attr::OBJID, 666)
            })
            .await;
        broker
            .send(MsgType::Status, add.seq, forger, |w| {
                w.put_u32(attr::STATUS, 0)
            })
            .await;
        for (seq, id) in [(add.seq, 100), (add.seq + 1, 101)] {
            if seq != add.seq {
                let next = broker.recv().await;
                assert_eq!((next.ty, next.seq), (Some(MsgType::AddObject), seq));
            }
            broker
                .send(MsgType::Data, seq, 0, |w| w.put_u32(attr::OBJID, id))
                .await;
            broker
                .send(MsgType::Status, seq, 0, |w| w.put_u32(attr::STATUS, 0))
                .await;
        }
        broker.invoke(1, 100, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await.2, 0, "ubusd's id is the one kept");
        broker.invoke(2, 666, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await.2, Status::NOT_FOUND.0);
    }

    #[tokio::test]
    async fn slow_calls_do_not_block_others() {
        let (mut broker, _server, _task) = start(example());
        broker.accept(2).await;
        broker
            .invoke(1, 100, "slow", &Table::new().with("ms", 300i32), false)
            .await;
        broker.invoke(2, 100, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await.0, 2);
        assert_eq!(broker.reply().await.0, 1);
    }

    #[tokio::test]
    async fn no_reply_calls_get_no_answer() {
        let (mut broker, _server, _task) = start(example());
        broker.accept(2).await;
        broker.invoke(1, 100, "echo", &Table::new(), true).await;
        broker.invoke(2, 100, "missing", &Table::new(), true).await;
        broker.invoke(3, 100, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await.0, 3);
    }

    #[tokio::test]
    async fn refused_object_spares_the_others() {
        let (mut broker, server, task) = start(example());
        broker.send(MsgType::Hello, 0, 1, |_| {}).await;
        let first = broker.recv().await;
        broker
            .send(MsgType::Status, first.seq, 0, |w| {
                w.put_u32(attr::STATUS, Status::INVALID_ARGUMENT.0 as u32)
            })
            .await;
        let second = broker.recv().await;
        assert_eq!(second.ty, Some(MsgType::AddObject));
        broker
            .send(MsgType::Data, second.seq, 0, |w| {
                w.put_u32(attr::OBJID, 101)
            })
            .await;
        broker
            .send(MsgType::Status, second.seq, 0, |w| {
                w.put_u32(attr::STATUS, 0)
            })
            .await;
        broker.invoke(1, 101, "noop", &Table::new(), false).await;
        let reply = broker.recv().await;
        assert_eq!(reply.attrs().unwrap().status, Some(0));
        assert!(!task.is_finished());
        assert!(
            !*server.registered().borrow(),
            "not every object is registered"
        );
    }

    #[tokio::test]
    async fn broker_hangup_ends_the_session() {
        let (mut broker, server, task) = start(example());
        broker.accept(2).await;
        let mut registered = server.registered();
        registered.wait_for(|r| *r).await.unwrap();
        drop(broker);
        assert!(matches!(task.await.unwrap(), Error::Closed));
        assert!(!*server.registered().borrow());
    }

    #[tokio::test]
    async fn registers_one_object_at_a_time() {
        let (mut broker, _server, _task) = start(example());
        broker.send(MsgType::Hello, 0, 1, |_| {}).await;
        let first = broker.recv().await;
        assert_eq!(first.ty, Some(MsgType::AddObject));
        let early = tokio::time::timeout(Duration::from_millis(100), broker.reader.next()).await;
        assert!(
            early.is_err(),
            "second ADD_OBJECT sent before the first was answered"
        );
    }

    #[tokio::test]
    async fn refuses_calls_past_the_limit() {
        let (mut broker, _server, _task) = start(example().max_calls(1));
        broker.accept(2).await;
        broker
            .invoke(1, 100, "slow", &Table::new().with("ms", 300i32), false)
            .await;
        broker.invoke(2, 100, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await, (2, None, Status::NO_MEMORY.0));
        assert_eq!(broker.reply().await, (1, None, 0));
        broker.invoke(3, 100, "echo", &Table::new(), false).await;
        assert_eq!(broker.reply().await.2, 0, "capacity frees up");
    }

    static STARTED: tokio::sync::Notify = tokio::sync::Notify::const_new();
    static FINISHED: AtomicBool = AtomicBool::new(false);

    #[tokio::test]
    async fn session_end_cancels_running_calls() {
        let server = Server::new().object(Object::new("test").method("late", |_| async {
            STARTED.notify_one();
            tokio::time::sleep(Duration::from_millis(200)).await;
            FINISHED.store(true, Ordering::SeqCst);
            Ok(None)
        }));
        let (mut broker, _server, task) = start(server);
        broker.accept(1).await;
        broker.invoke(1, 100, "late", &Table::new(), false).await;
        tokio::time::timeout(Duration::from_secs(1), STARTED.notified())
            .await
            .expect("handler never started");
        drop(broker);
        assert!(matches!(task.await.unwrap(), Error::Closed));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !FINISHED.load(Ordering::SeqCst),
            "handler outlived its session"
        );
    }

    #[tokio::test]
    async fn dropping_the_server_future_clears_registration() {
        let (mut broker, server, task) = start(example());
        broker.accept(2).await;
        let mut registered = server.registered();
        registered.wait_for(|r| *r).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!*registered.borrow_and_update());
    }

    #[tokio::test]
    async fn another_session_ending_keeps_readiness() {
        let (mut broker, server, _task) = start(example());
        broker.accept(2).await;
        let mut registered = server.registered();
        registered.wait_for(|r| *r).await.unwrap();
        // A second connection that never gets past HELLO, then is dropped.
        let (ours, _theirs) = tokio::io::duplex(1024);
        let second = tokio::spawn({
            let server = Arc::clone(&server);
            async move { server.serve(ours).await }
        });
        tokio::task::yield_now().await;
        second.abort();
        assert!(second.await.unwrap_err().is_cancelled());
        assert!(*server.registered().borrow());
    }

    #[tokio::test]
    async fn finished_calls_free_their_slot_before_refusal() {
        let server = example().max_calls(1);
        let mut session = Session::new(&server);
        session.by_id.insert(100, 0);
        let invoke = |seq| {
            let mut b = Builder::new(MsgType::Invoke, seq, 0x99);
            let w = b.writer();
            w.put_u32(attr::OBJID, 100);
            w.put_str(attr::METHOD, "echo");
            w.put(attr::DATA, &[]);
            b.finish().unwrap()
        };
        let mut stream = invoke(1);
        stream.extend(invoke(2));
        let mut reader = Reader::new(&stream[..]);
        let mut out = Vec::new();
        session
            .invoke(&reader.next().await.unwrap(), &mut out)
            .await
            .unwrap();
        // Let the first call finish without the session joining it.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        session
            .invoke(&reader.next().await.unwrap(), &mut out)
            .await
            .unwrap();
        let mut replies = Reader::new(&out[..]);
        let mut statuses = Vec::new();
        while let Ok(msg) = replies.next().await {
            if msg.ty == Some(MsgType::Status) {
                statuses.push((msg.seq, msg.attrs().unwrap().status.unwrap()));
            }
        }
        assert_eq!(
            statuses,
            [(1, 0)],
            "first call's reply, second still running"
        );
        assert_eq!(session.calls.len(), 1);
    }

    #[tokio::test]
    async fn unencodable_signature_spares_the_others() {
        let server = Server::new()
            .object(Object::new("test").method_with_args(
                "m",
                [("x".repeat(70_000), ArgType::Int32)],
                |_| async { Ok(None) },
            ))
            .object(Object::new("test.ok").method("noop", |_| async { Ok(None) }));
        let (mut broker, server, task) = start(server);
        let objects = broker.accept(1).await;
        assert_eq!(objects[0].0, "test.ok");
        broker.invoke(1, 100, "noop", &Table::new(), false).await;
        assert_eq!(broker.reply().await, (1, None, 0));
        assert!(!task.is_finished());
        assert!(!*server.registered().borrow());
    }
}
