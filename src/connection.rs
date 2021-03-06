//! Deals with creating and using connections to dbus-daemon.  The primary
//! type of interest is the Connection struct
//! 
//! # Examples
//! ```
//! use dbus_bytestream::connection::Connection;
//! use dbus_bytestream::message;
//!
//! let mut conn = Connection::connect_system().unwrap();
//! let mut msg = message::create_method_call(
//!     "org.freedesktop.DBus", // destination
//!     "/org/freedesktop/DBus", // path
//!     "org.freedesktop.DBus", //interface
//!     "ListNames" // method
//! );
//! let reply = conn.call_sync(&mut msg);
//! println!("{:?}", reply);
//! ```

use std::env;
use std::net::TcpStream;
use std::collections::HashMap;
use std::io;
use std::io::{Read,Write};
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use libc;

use unix_socket::UnixStream;
use rustc_serialize::hex::ToHex;
use dbus_serialize::types::{Value,BasicValue};
use dbus_serialize::decoder::DBusDecoder;

use address;
use address::ServerAddress;
use message;
use message::{Message,HeaderFieldName,MessageBuf};
use demarshal::{demarshal,DemarshalError};
use marshal::Marshal;

trait StreamSocket : Read + Write { }
impl<T: Read + Write> StreamSocket for T {}

enum Socket {
    Tcp(TcpStream),
    Uds(UnixStream)
}

pub struct Connection {
    sock: Socket,
    next_serial: u32,
    queue: Vec<Message>,
}

#[derive(Debug)]
pub enum Error {
    Disconnected,
    IOError(io::Error),
    DemarshalError(DemarshalError),
    AddressError(address::ServerAddressError),
    BadData,
    AuthFailed,
    NoEnvironment,
}

impl From<io::Error> for Error {
    fn from(x: io::Error) -> Self {
        Error::IOError(x)
    }
}

impl From<DemarshalError> for Error {
    fn from(x: DemarshalError) -> Self {
        Error::DemarshalError(x)
    }
}

impl From<address::ServerAddressError> for Error {
    fn from(x: address::ServerAddressError) -> Self {
        Error::AddressError(x)
    }
}

fn read_exactly(sock: &mut StreamSocket, buf: &mut Vec<u8>, len: usize) -> Result<(),Error> {
    buf.truncate(0);
    buf.reserve(len);
    if try!(sock.take(len as u64).read_to_end(buf)) != len {
        return Err(Error::Disconnected);
    }
    Ok(())
}

fn read_line(sock: &mut StreamSocket) -> Result<String,Error> {
    let mut line = "".to_string();
    let mut last = '\0';

    loop {
        let mut buf = vec![0];
        match sock.read(&mut buf) {
            Ok(x) if x > 0 => (),
            _ => return Err(Error::Disconnected)
        };
        let chr = buf[0] as char;
        line.push(chr);
        if chr == '\n' && last == '\r' {
            break;
        }
        last = chr;
    }
    Ok(line)
}

impl Connection {
    fn get_sock(&mut self) -> &mut StreamSocket {
        match self.sock {
            Socket::Tcp(ref mut x) => x,
            Socket::Uds(ref mut x) => x
        }
    }

    fn send_nul_byte(&mut self) -> Result<(),Error> {
        // Send NUL byte
        let sock = self.get_sock();
        let buf = vec![0];
        try!(sock.write_all(&buf));
        Ok(())
    }

    fn auth_anonymous(&mut self) -> Result<(),Error> {
        let sock = self.get_sock();

        try!(sock.write_all(b"AUTH ANONYMOUS 6c69626462757320312e382e3132\r\n"));

        // Read response
        let resp = try!(read_line(sock));
        if !resp.starts_with("OK ") {
            return Err(Error::AuthFailed);
        }

        // Ready for action
        try!(sock.write_all(b"BEGIN\r\n"));
        Ok(())
    }

    fn auth_external(&mut self) -> Result<(),Error> {
        let sock = self.get_sock();

        let uid = unsafe {
            libc::funcs::posix88::unistd::getuid()
        };
        let uid_str = uid.to_string();
        let uid_hex = uid_str.into_bytes().to_hex();
        let cmd = "AUTH EXTERNAL ".to_string() + &uid_hex + "\r\n";
        try!(sock.write_all(&cmd.into_bytes()));

        // Read response
        let resp = try!(read_line(sock));
        if !resp.starts_with("OK ") {
            return Err(Error::AuthFailed);
        }

        // Ready for action
        try!(sock.write_all(b"BEGIN\r\n"));
        Ok(())
    }

    fn say_hello(&mut self) -> Result<(String),Error> {
        let mut msg = message::create_method_call("org.freedesktop.DBus",
                                                  "/org/freedesktop/DBus",
                                                  "org.freedesktop.DBus",
                                                  "Hello");
        match try!(self.call_sync(&mut msg)).get(0) {
            Some(&Value::BasicValue(BasicValue::String(ref x))) => Ok(x.to_string()),
            _ => Err(Error::BadData)
        }
    }

    fn connect_addr(addr: ServerAddress) -> Result<Connection,Error> {
        match addr {
            ServerAddress::Unix(unix) => Self::connect_uds(unix.path()),
        }
    }

    /// Connects to a DBus address string.
    pub fn connect(addr: &str) -> Result<Connection, Error> {
        Self::connect_addr(try!(ServerAddress::from_str(addr)))
    }

    /// Connects to the system bus.
    ///
    /// The address is specified by the environment variable
    /// DBUS_SYSTEM_BUS_ADDRESS or "unix:path=/var/run/dbus/system_bus_socket" if unset.
    pub fn connect_system() -> Result<Connection, Error> {
        let default = "unix:path=/var/run/dbus/system_bus_socket";
        if let Ok(e) = env::var("DBUS_SYSTEM_BUS_ADDRESS") {
            Self::connect(&e)
        } else {
            Self::connect(default)
        }
    }

    /// Connects to the session bus.
    ///
    /// The address is specified by the environment variable DBUS_SESSION_BUS_ADDRESS.
    pub fn connect_session() -> Result<Connection, Error> {
        if let Ok(e) = env::var("DBUS_SESSION_BUS_ADDRESS") {
            Self::connect(&e)
        } else {
            Err(Error::NoEnvironment)
        }
    }

    /// Creates a Connection object using a UNIX domain socket as the transport.  The addr is the
    /// path to connect to.  Abstract paths can be used by passing a NUL byte as the first byte of
    /// addr.
    pub fn connect_uds<P: AsRef<Path>>(addr: P) -> Result<Connection,Error> {
        let sock = try!(UnixStream::connect(addr));
        let mut conn = Connection {
            sock: Socket::Uds(sock),
            queue: Vec::new(),
            next_serial: 1
        };

        try!(conn.send_nul_byte());
        try!(conn.auth_external());
        try!(conn.say_hello());
        Ok(conn)
    }

    /// Creates a Connection object using a TCP socket as the transport.  The addr is the host and
    /// port to connect to.
    pub fn connect_tcp(addr: &str) -> Result<Connection,Error> {
        let sock = try!(TcpStream::connect(addr));
        let mut conn = Connection {
            sock: Socket::Tcp(sock),
            queue: Vec::new(),
            next_serial: 1
        };

        try!(conn.send_nul_byte());
        try!(conn.auth_anonymous());
        try!(conn.say_hello());
        Ok(conn)
    }

    /// Sends a message over the connection.  The MessageBuf can be created by one of the functions
    /// from the message module, such as message::create_method_call .  On success, returns the
    /// serial number of the outgoing message so that the reply can be identified.
    pub fn send(&mut self, mbuf: &mut MessageBuf) -> Result<u32, Error> {
        let mut msg = &mut mbuf.0;
        // A minimum header with no body is 16 bytes
        let mut len = msg.len() as u32;
        if len < 16 {
            return Err(Error::BadData);
        }

        // Get the current length from the message, which only include the length of the header.
        // That field should actually be the length of only the body, so update that now
        let old_len = message::get_length(msg);
        len -= old_len;
        let mut buf = Vec::new();
        len.dbus_encode(&mut buf);
        // Update the message with a correct serial number, as well
        let this_serial = self.next_serial;
        self.next_serial += 1;
        this_serial.dbus_encode(&mut buf);
        message::set_length(msg, &buf);

        let sock = self.get_sock();
        try!(sock.write_all(msg));
        Ok(this_serial)
    }

    /// Sends a message over a connection and block until a reply is received.  This is only valid
    /// for method calls.  Returns the sequence of Value objects that is the body of the method
    /// return.
    ///
    /// # Panics
    /// Calling this function with a MessageBuf for other than METHOD_CALL or with the
    /// NO_REPLY_EXPECTED flag set is a programming error and will panic.
    pub fn call_sync(&mut self, mbuf: &mut MessageBuf) -> Result<Vec<Value>,Error> {
        // XXX: assert that this is a method call with reply
        let serial = try!(self.send(mbuf));
        // We need a local queue so that read_msg doesn't just give us
        // the same one over and over
        let mut queue = Vec::new();
        loop {
            let mut msg = try!(self.read_msg());
            match msg.headers.remove(&(HeaderFieldName::ReplySerial as u8)) {
                Some(Value::Variant(x)) => {
                    let obj : Value = *x.object;
                    let reply_serial : u32 = DBusDecoder::decode(obj).unwrap();
                    if reply_serial == serial {
                        // Move our queued messages into the Connection's queue
                        for _ in 0..queue.len() {
                            self.queue.push(queue.remove(0));
                        }
                        return Ok(msg.body);
                    }
                }
                _ => ()
            };
            queue.push(msg);
        }
    }

    /// Blocks until a message comes in from the message bus.  The received message is returned.
    pub fn read_msg(&mut self) -> Result<Message,Error> {
        match self.queue.get(0) {
            Some(_) => return Ok(self.queue.remove(0)),
            _ => ()
        };
        let mut buf = Vec::new();
        let sock = self.get_sock();

        // Read and demarshal the fixed portion of the header
        try!(read_exactly(sock, &mut buf, 12));
        let mut offset = 0;
        let mut sig = "(yyyyuu)".to_string();
        let header = match try!(demarshal(&mut buf, &mut offset, &mut sig)) {
            Value::Struct(x) => x,
            x => panic!("Demarshal didn't return what we asked for: {:?}", x)
        };

        let mut v = header.objects;
        let mut msg : Message = Default::default();
        let endian : u8 = DBusDecoder::decode(v.remove(0)).unwrap();
        if endian == 'B' as u8 {
            msg.big_endian = true;
        }
        msg.message_type = message::MessageType(DBusDecoder::decode(v.remove(0)).unwrap());
        msg.flags = DBusDecoder::decode::<u8>(v.remove(0)).unwrap();
        msg.version = DBusDecoder::decode::<u8>(v.remove(0)).unwrap();
        let body_len = DBusDecoder::decode::<u32>(v.remove(0)).unwrap();
        msg.serial = DBusDecoder::decode::<u32>(v.remove(0)).unwrap();

        // Read array length
        try!(read_exactly(sock, &mut buf, 4));
        // demarshal consumes the buf, so save a copy for when we demarshal the entire array
        let mut buf_copy = buf.clone();
        offset = 12;
        sig = "u".to_string();
        let data = demarshal(&mut buf, &mut offset, &mut sig).ok().unwrap();
        let arr_len = DBusDecoder::decode::<u32>(data).unwrap() as usize;

        // Make buf_copy big enough for the entire array, and fill it
        buf_copy.reserve(arr_len);
        if try!(sock.take(arr_len as u64).read_to_end(&mut buf_copy)) != arr_len {
            return Err(Error::Disconnected);
        };

        offset = 12;
        sig = "a(yv)".to_string();
        let header_fields = match try!(demarshal(&mut buf_copy, &mut offset, &mut sig)) {
            Value::Array(x) => x,
            x => panic!("Demarshal didn't return what we asked for: {:?}", x)
        };

        msg.headers = HashMap::new();
        for i in header_fields.objects {
            let mut st = match i {
                Value::Struct(x) => x,
                x => panic!("Demarshal didn't return what we asked for: {:?}", x)
            };
            let val = st.objects.remove(1);
            let code = DBusDecoder::decode::<u8>(st.objects.remove(0)).unwrap();
            msg.headers.insert(code, val);
        }

        // Read the padding, if any
        let trailing_pad = 8 - (offset % 8);
        if trailing_pad % 8 != 0 {
            try!(read_exactly(sock, &mut buf, trailing_pad));
        }

        // Finally, read the entire body
        if body_len > 0 {
            let v = match msg.headers.get(&(HeaderFieldName::Signature as u8)) {
                Some(&Value::Variant(ref x)) => x,
                _ => return Err(Error::DemarshalError(DemarshalError::BadSignature))
            };

            let sigval = match v.object.deref() {
                &Value::BasicValue(BasicValue::Signature(ref x)) => x,
                _ => return Err(Error::DemarshalError(DemarshalError::BadSignature))
            };

            let mut body = Vec::new();
            try!(read_exactly(sock, &mut body, body_len as usize));

            let mut sig = "(".to_string() + &sigval.0 + ")";
            offset = 0;
            let objs = match try!(demarshal(&mut body, &mut offset, &mut sig)) {
                Value::Struct(x) => x.objects,
                x => panic!("Didn't get a struct: {:?}", x)
            };
            for x in objs {
                msg.body.push(x);
            }
        }

        Ok(msg)
    }
}

#[cfg(dbus)]
#[cfg(test)]
fn validate_connection(conn: &mut Connection) {
    let mut msg = message::create_method_call("org.freedesktop.DBus", "/org/freedesktop/DBus",
                                          "org.freedesktop.DBus", "ListNames");
    let resp = conn.call_sync(&mut msg).unwrap();
    println!("ListNames: {:?}", resp);
}

#[cfg(dbus)]
#[test]
fn test_connect() {
    let mut conn = Connection::connect_uds("/var/run/dbus/system_bus_socket").unwrap();
    validate_connection(&mut conn);
}

#[cfg(dbus)]
#[test]
fn test_connect_system() {
    let mut conn = Connection::connect_system().unwrap();
    validate_connection(&mut conn);
}

#[cfg(dbus)]
#[test]
fn test_connect_session() {
    let mut conn = Connection::connect_session().unwrap();
    validate_connection(&mut conn);
}

#[cfg(dbus)]
#[test]
fn test_tcp() {
    let mut conn = Connection::connect_tcp("localhost:12345").unwrap();
    let mut msg = message::create_method_call("org.freedesktop.DBus", "/org/freedesktop/DBus",
                                          "org.freedesktop.DBus", "ListNames");
    conn.send(&mut msg).ok();
    let msg = conn.read_msg().unwrap();
    println!("{:?}", msg.body);
    //loop {
    //    conn.read_msg().unwrap();
    //}
}
