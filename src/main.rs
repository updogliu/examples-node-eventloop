/// Think of this function as the javascript program you have written
fn javascript() {
    println!("Thread: {}. First call to read test.txt", current());
    Fs::read("test.txt", |result| {
        let text = result.into_string().unwrap();
        let len = text.len();
        println!("Thread: {}. First count: {} characters.", current(), len);

        println!("Thread: {}. I want to encrypt something.", current());
        Crypto::encrypt(text.len(), |result| {
            let n = result.into_int().unwrap();
            println!("Thread: {}. \"Encrypted\" number is: {}", current(), n);
        })
    });

    // let's read the file again and display the text
    println!("Thread: {}. Second call to read test.txt", current());
    Fs::read("test.txt", |result| {
        let text = result.into_string().unwrap();
        let len = text.len();
        println!("Thread: {}. Second count: {} characters.", current(), len);

        // aaand one more time but not in parallell.
        println!("Thread: {}. Third call to read test.txt", current());
        Fs::read("test.txt", |result| {
            let text = result.into_string().unwrap();
            println!(
                "Thread: {}. The file contains the following text:\n\n\"{}\"\n",
                current(),
                text
            );
        });
    });

    Io::timeout(3000, |_res| {
        println!("Thread: {}.Timer1 timed out", current());
        Io::timeout(1500, |_res| {
            println!("Thread: {}. Timer3(nested) timed out", current());
        });
    });

    Io::http_get_slow("http//www.google.com", 5000, |result| {
        let result = result.into_string().unwrap();
        println!("\n===== START WEB RESPONSE =====");
        println!("{}", result);
        println!("===== END WEB RESPONSE =====");
    });
}

fn current() -> String {
    thread::current().name().unwrap().to_string()
}

fn main() {
    let mut rt = Runtime::new();
    rt.run(javascript);
}

// ===== THIS IS OUR "NODE LIBRARY" =====
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{self, JoinHandle};

static mut RUNTIME: usize = 0;

type Callback = Box<FnOnce(Js)>;

struct Event {
    task: Box<Fn() -> Js + Send + 'static>,
    callback_id: usize,
    kind: EventKind,
}

enum EventKind {
    FileRead,
    Encrypt,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use EventKind::*;
        match self {
            FileRead => write!(f, "File read"),
            Encrypt => write!(f, "Encrypt"),
        }
    }
}

#[derive(Debug)]
enum Js {
    Undefined,
    String(String),
    Int(usize),
}

impl Js {
    /// Convenience method since we know the types
    fn into_string(self) -> Option<String> {
        match self {
            Js::String(s) => Some(s),
            _ => None,
        }
    }

    /// Convenience method since we know the types
    fn into_int(self) -> Option<usize> {
        match self {
            Js::Int(n) => Some(n),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct NodeThread {
    handle: JoinHandle<()>,
    sender: Sender<Event>,
}

struct Runtime {
    thread_pool: Box<[NodeThread]>,
    available: Vec<usize>,
    callback_queue: HashMap<usize, Callback>,
    identity_token: usize,
    refs: usize,
    threadp_reciever: Receiver<(usize, usize, Js)>,
    epoll_reciever: Receiver<usize>,
    epoll_queue: i32,
    epoll_pending: usize,
    epoll_starter: Sender<usize>,
    epoll_event_cb_map: HashMap<i64, usize>,
}

impl Runtime {
    fn new() -> Self {
        // ===== THE REGULAR THREADPOOL =====
        let (threadp_sender, threadp_reciever) = channel::<(usize, usize, Js)>();
        let mut threads = Vec::with_capacity(4);
        for i in 0..4 {
            let (evt_sender, evt_reciever) = channel::<Event>();
            let threadp_sender = threadp_sender.clone();
            let handle = thread::Builder::new()
                .name(format!("pool{}", i))
                .spawn(move || {
                    while let Ok(event) = evt_reciever.recv() {
                        println!(
                            "Thread {}, recived a task of type: {}",
                            thread::current().name().unwrap(),
                            event.kind,
                        );
                        let res = (event.task)();
                        println!(
                            "Thread {}, finished running a task of type: {}.",
                            thread::current().name().unwrap(),
                            event.kind
                        );
                        threadp_sender.send((i, event.callback_id, res)).unwrap();
                    }
                })
                .expect("Couldn't initialize thread pool.");

            let node_thread = NodeThread {
                handle,
                sender: evt_sender,
            };

            threads.push(node_thread);
        }

        // ===== EPOLL THREAD =====
        // Only wakes up when there is a task ready
        let (epoll_sender, epoll_reciever) = channel::<usize>();
        let (epoll_start_sender, epoll_start_reciever) = channel::<usize>();
        let queue = minimio::queue().expect("Error creating epoll queue");
        thread::Builder::new()
            .name("epoll".to_string())
            .spawn(move || loop {
                let mut changes = vec![];
                while let Ok(current_event_count) = epoll_start_reciever.recv() {
                    if changes.len() < current_event_count {
                        let missing = current_event_count - changes.len();
                        (0..missing).for_each(|_| changes.push(minimio::Event::default()));
                    }
                    match minimio::poll(queue, changes.as_mut_slice(), 0, None) {
                        Ok(v) if v > 0 => {
                            for i in 0..v {
                                let event = changes.get(i).expect("No events in event list.");
                                println!(
                                    "Thread {}: epoll event {} is ready",
                                    current(),
                                    event.ident
                                );
                                epoll_sender.send(event.ident as usize).unwrap();
                                changes.remove(i);
                            }
                        }
                        Err(e) => panic!("{:?}", e),
                        _ => (),
                    }
                }
            })
            .expect("Error creating epoll thread");

        Runtime {
            thread_pool: threads.into_boxed_slice(),
            available: (0..4).collect(),
            callback_queue: HashMap::new(),
            identity_token: 0,
            refs: 0,
            threadp_reciever,
            epoll_reciever,
            epoll_queue: queue,
            epoll_pending: 0,
            epoll_starter: epoll_start_sender,
            epoll_event_cb_map: HashMap::new(),
        }
    }

    /// This is the event loop
    fn run(&mut self, f: impl Fn()) {
        let rt_ptr: *mut Runtime = self;
        unsafe { RUNTIME = rt_ptr as usize };

        // First we run our "main" function
        f();

        // The we check that we we don't have any more
        while self.refs > 0 {
            // Check if we have any timer events that have expired

            // First poll any epoll/kqueue
            if let Ok(event_id) = self.epoll_reciever.try_recv() {
                let id = self
                    .epoll_event_cb_map
                    .get(&(event_id as i64))
                    .expect("Event not in event map.");
                let callback_id = *id;
                self.epoll_event_cb_map.remove(&(event_id as i64));

                let cb = self.callback_queue.remove(&callback_id).unwrap();
                cb(Js::Undefined);
                self.refs -= 1;
                self.epoll_pending -= 1;
            }

            // then check if there is any results from the threadpool
            if let Ok((thread_id, callback_id, data)) = self.threadp_reciever.try_recv() {
                let cb = self.callback_queue.remove(&callback_id).unwrap();
                cb(data);
                self.refs -= 1;
                self.available.push(thread_id);
            }

            // Let the OS have a time slice of our thread so we don't busy loop
            thread::sleep(std::time::Duration::from_millis(1));
        }
        println!("FINISHED");
    }

    fn schedule(&mut self) -> usize {
        match self.available.pop() {
            Some(thread_id) => thread_id,
            // We would normally queue this
            None => panic!("Out of threads."),
        }
    }

    /// If we hit max we just wrap around
    fn generate_identity(&mut self) -> usize {
        self.identity_token = self.identity_token.wrapping_add(1);
        self.identity_token
    }

    /// Adds a callback to the queue and returns the key
    fn add_callback(&mut self, cb: impl FnOnce(Js) + 'static) -> usize {
        // this is the happy path
        let ident = self.generate_identity();
        let boxed_cb = Box::new(cb);
        let taken = self.callback_queue.contains_key(&ident);

        // if there is a collision or the identity is already there we loop until we find a new one
        // we don't cover the case where there are `usize::MAX` number of callbacks waiting since
        // that if we're fast and queue a new event every nanosecond that will still take 585.5 years
        // to do on a 64 bit system.
        if !taken {
            self.callback_queue.insert(ident, boxed_cb);
            ident
        } else {
            loop {
                let possible_ident = self.generate_identity();
                if self.callback_queue.contains_key(&possible_ident) {
                    self.callback_queue.insert(possible_ident, boxed_cb);
                    break possible_ident;
                }
            }
        }
    }

    fn register_io(&mut self, event: minimio::Event, cb: impl FnOnce(Js) + 'static) {
        let cb_id = self.add_callback(cb) as i64;
        println!(
            "Thread {}: Event with id: {} registered.",
            current(),
            event.ident
        );
        self.epoll_event_cb_map
            .insert(event.ident as i64, cb_id as usize);

        minimio::add_event(self.epoll_queue, &[event.clone()], 0)
            .expect("Error adding event to queue.");

        self.refs += 1;
        self.epoll_pending += 1;
        self.epoll_starter
            .send(self.epoll_pending)
            .expect("Sending to epoll_starter.");
    }

    fn register_work(
        &mut self,
        task: impl Fn() -> Js + Send + 'static,
        kind: EventKind,
        cb: impl FnOnce(Js) + 'static,
    ) {
        let callback_id = self.add_callback(cb);

        let event = Event {
            task: Box::new(task),
            callback_id,
            kind,
        };

        // we are not going to implement a real scheduler here, just a LIFO queue
        let available = self.schedule();
        self.thread_pool[available].sender.send(event).unwrap();
        self.refs += 1;
    }
}

// ===== THIS IS PLUGINS CREATED IN C++ FOR THE NODE RUNTIME OR PART OF THE RUNTIME ITSELF =====
// The pointer dereferencing of our runtime is not striclty needed but is mostly for trying to
// emulate a bit of the same feeling as when you use modules in javascript. We could pass the runtime in
// as a reference to our startup function.

struct Crypto;

impl Crypto {
    fn encrypt(n: usize, cb: impl Fn(Js) + 'static + Clone) {
        let work = move || {
            fn fibonacchi(n: usize) -> usize {
                match n {
                    0 => 0,
                    1 => 1,
                    _ => fibonacchi(n - 1) + fibonacchi(n - 2),
                }
            }

            let fib = fibonacchi(n);
            Js::Int(fib)
        };

        let rt = unsafe { &mut *(RUNTIME as *mut Runtime) };
        rt.register_work(work, EventKind::Encrypt, cb);
    }
}

struct Fs;
impl Fs {
    fn read(path: &'static str, cb: impl Fn(Js) + 'static) {
        let work = move || {
            // Let's simulate that there is a very large file we're reading allowing us to actually
            // observe how the code is executed
            thread::sleep(std::time::Duration::from_secs(2));
            let mut buffer = String::new();
            fs::File::open(&path)
                .unwrap()
                .read_to_string(&mut buffer)
                .unwrap();
            Js::String(buffer)
        };
        let rt = unsafe { &mut *(RUNTIME as *mut Runtime) };
        rt.register_work(work, EventKind::FileRead, cb);
    }
}

// ===== THIS IS OUR EPOLL/KQUEUE/IOCP LIBRARY =====
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};

struct Io;
impl Io {
    pub fn timeout(ms: u32, cb: impl Fn(Js) + 'static) {
        let event = minimio::event_timeout(i64::from(ms));

        let rt: &mut Runtime = unsafe { &mut *(RUNTIME as *mut Runtime) };
        rt.register_io(event, cb);
    }

    pub fn http_get_slow(url: &str, delay_ms: u32, cb: impl Fn(Js) + 'static + Clone) {
        // Don't worry, http://slowwly.robertomurray.co.uk is a site for simulating a delayed
        // response from a server. Perfect for our use case.
        let mut stream: TcpStream = TcpStream::connect("slowwly.robertomurray.co.uk:80").unwrap();
        let request = format!(
            "GET /delay/{}/url/http://{} HTTP/1.1\r\n\
             Host: slowwly.robertomurray.co.uk\r\n\
             Connection: close\r\n\
             \r\n",
            delay_ms, url
        );

        stream
            .write_all(request.as_bytes())
            .expect("Error writing to stream");
        stream
            .set_nonblocking(true)
            .expect("set_nonblocking call failed");
        let fd = stream.as_raw_fd();

        let event = minimio::event_read(fd);

        let wrapped = move |_n| {
            let mut stream = stream;
            let mut buffer = String::new();
            stream
                .read_to_string(&mut buffer)
                .expect("Error reading from stream.");
            cb(Js::String(buffer));
        };

        let rt: &mut Runtime = unsafe { &mut *(RUNTIME as *mut Runtime) };
        rt.register_io(event, wrapped);
    }
    /// URl is in www.google.com format, i.e. only the host name, we can't
    /// request paths at this point
    pub fn http_get(url: &str, cb: impl Fn(Js) + 'static + Clone) {
        let url_port = format!("{}:80", url);
        let mut stream: TcpStream = TcpStream::connect(&url_port).unwrap();
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: {}\r\n\
             Connection: close\r\n\
             \r\n",
            url
        );

        stream
            .write_all(request.as_bytes())
            .expect("Error writing to stream");
        stream
            .set_nonblocking(true)
            .expect("set_nonblocking call failed");
        let fd = stream.as_raw_fd();

        let event = minimio::event_read(fd);

        let wrapped = move |_n| {
            let mut stream = stream;
            let mut buffer = String::new();
            stream
                .read_to_string(&mut buffer)
                .expect("Error reading from stream.");
            // The way we do this we know it's a redirect so we grab the location header and
            // get that webpage instead
            cb(Js::String(buffer));
        };

        let rt: &mut Runtime = unsafe { &mut *(RUNTIME as *mut Runtime) };
        rt.register_io(event, wrapped);
    }
}

/// As you'll see the system calls for interacting with Epoll, Kqueue and IOCP is highly
/// platform specific. The Rust community has already abstracted this away in the `mio` crate
/// but since we want to see what really goes on under the hood we implement a sort of mini-mio
/// library ourselves.
mod minimio {
    use super::*;
    pub fn queue() -> io::Result<i32> {
        if cfg!(target_os = "macos") {
            macos::kqueue()
        } else {
            unimplemented!()
        }
    }

    #[cfg(target_os = "macos")]
    pub type Event = macos::ffi::Kevent;

    pub fn poll(
        queue: i32,
        changelist: &mut [Event],
        timeout: usize,
        max_events: Option<i32>,
    ) -> io::Result<usize> {
        if cfg!(target_os = "macos") {
            macos::kevent(queue, &[], changelist, timeout)
        } else {
            unimplemented!()
        }
    }

    /// Timeout of 0 means no timeout
    pub fn add_event(queue: i32, event_list: &[Event], timeout_ms: usize) -> io::Result<usize> {
        if cfg!(target_os = "macos") {
            macos::kevent(queue, event_list, &mut [], timeout_ms)
        } else {
            unimplemented!()
        }
    }

    pub fn event_timeout(timeout_ms: i64) -> Event {
        if cfg!(target_os = "macos") {
            Event {
                ident: 0,
                filter: unsafe { macos::EVFILT_TIMER },
                flags: unsafe { macos::EV_ADD | macos::EV_ENABLE | macos::EV_ONESHOT },
                fflags: 0,
                data: timeout_ms,
                udata: 0,
                ext: [0, 0],
            }
        } else {
            unimplemented!()
        }
    }

    pub fn event_read(fd: RawFd) -> Event {
        if cfg!(target_os = "macos") {
            Event {
                ident: fd as u64,
                filter: macos::EVFILT_READ,
                flags: macos::EV_ADD | macos::EV_ENABLE | macos::EV_ONESHOT,
                fflags: 0,
                data: 0,
                udata: 0,
                ext: [0, 0],
            }
        } else {
            unimplemented!()
        }
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;
        use ffi::*;

        // Shamelessly stolen from the libc wrapper found at:
        // https://github.com/rust-lang/libc/blob/c8aa8ec72d631bc35099bcf5d634cf0a0b841be0/src/unix/bsd/apple/mod.rs#L2447
        pub const EVFILT_TIMER: i16 = -7;
        pub const EVFILT_READ: i16 = -1;
        pub const EV_ADD: u16 = 0x1;
        pub const EV_ENABLE: u16 = 0x4;
        pub const EV_ONESHOT: u16 = 0x10;

        pub mod ffi {
            #[derive(Debug, Clone, Default)]
            #[repr(C)]
            // https://github.com/rust-lang/libc/blob/c8aa8ec72d631bc35099bcf5d634cf0a0b841be0/src/unix/bsd/apple/mod.rs#L497
            // https://github.com/rust-lang/libc/blob/c8aa8ec72d631bc35099bcf5d634cf0a0b841be0/src/unix/bsd/apple/mod.rs#L207
            pub struct Kevent {
                pub ident: u64,
                pub filter: i16,
                pub flags: u16,
                pub fflags: u32,
                pub data: i64,
                pub udata: u64,
                pub ext: [u64; 2],
            }
            #[link(name = "c")]
            extern "C" {
                /// Returns: positive: file descriptor, negative: error
                pub(super) fn kqueue() -> i32;
                /// Returns: nothing, all non zero return values is an error
                pub(super) fn kevent(
                    kq: i32,
                    changelist: *const Kevent,
                    nchanges: i32,
                    eventlist: *mut Kevent,
                    nevents: i32,
                    timeout: usize,
                ) -> i32;
            }
        }
        pub fn timeout_event(timer: i64) -> ffi::Kevent {
            ffi::Kevent {
                ident: 1,
                filter: EVFILT_TIMER,
                flags: EV_ADD | EV_ENABLE | EV_ONESHOT,
                fflags: 0,
                data: timer,
                udata: 0,
                ext: [0, 0],
            }
        }

        pub fn kqueue() -> io::Result<i32> {
            let fd = unsafe { ffi::kqueue() };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(fd)
        }

        pub fn kevent(
            kq: RawFd,
            cl: &[Kevent],
            el: &mut [Kevent],
            timeout: usize,
        ) -> io::Result<usize> {
            let res = unsafe {
                let kq = kq as i32;
                let cl_len = cl.len() as i32;
                let el_len = el.len() as i32;
                ffi::kevent(kq, cl.as_ptr(), cl_len, el.as_mut_ptr(), el_len, timeout)
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(res as usize)
        }
    }
}