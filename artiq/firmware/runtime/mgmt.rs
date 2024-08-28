use log::{self, LevelFilter};
use core::cell::Cell;
use core::cell::RefCell;

use board_artiq::drtio_routing;
use io::{ProtoRead, Write, Error as IoError};
use mgmt_proto::*;
use sched::{Io, TcpListener, TcpStream, Error as SchedError};
use sched::{Io, Mutex, TcpListener, TcpStream, Error as SchedError};
use urc::Urc;

impl From<SchedError> for Error<SchedError> {
    fn from(value: SchedError) -> Error<SchedError> {
        Error::Io(IoError::Other(value))
    }
}

mod local_coremgmt {
    use alloc::{string::String, vec::Vec};
    use log::LevelFilter;

    use board_misoc::{config, spiflash};
    use io::{Write, ProtoWrite, Error as IoError};
    use logger_artiq::BufferLogger;
    use mgmt_proto::{Error, Reply};
    use sched::{Io, TcpStream, Error as SchedError};


    pub fn get_log(io: &Io, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        BufferLogger::with(|logger| {
            let mut buffer = io.until_ok(|| logger.buffer())?;
            Reply::LogContent(buffer.extract()).write_to(stream)
        })?;
        Ok(())
    }

    pub fn clear_log(io: &Io, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        BufferLogger::with(|logger| -> Result<(), IoError<SchedError>> {
            let mut buffer = io.until_ok(|| logger.buffer())?;
            Ok(buffer.clear())
        })?;

        Reply::Success.write_to(stream)?;
        Ok(())
    }

    pub fn pull_log(io: &Io, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        BufferLogger::with(|logger| -> Result<(), IoError<SchedError>> {
            loop {
                // Do this *before* acquiring the buffer, since that sets the log level
                // to OFF.
                let log_level = log::max_level();

                let mut buffer = io.until_ok(|| logger.buffer())?;
                if buffer.is_empty() { continue }

                stream.write_string(buffer.extract())?;

                if log_level == LevelFilter::Trace {
                    // Hold exclusive access over the logger until we get positive
                    // acknowledgement; otherwise we get an infinite loop of network
                    // trace messages being transmitted and causing more network
                    // trace messages to be emitted.
                    //
                    // Any messages unrelated to this management socket that arrive
                    // while it is flushed are lost, but such is life.
                    stream.flush()?;
                }

                // Clear the log *after* flushing the network buffers, or we're just
                // going to resend all the trace messages on the next iteration.
                buffer.clear();
            }
        })?;
        Ok(())
    }

    pub fn set_log_filter(_io: &Io, stream: &mut TcpStream, level: LevelFilter) -> Result<(), Error<SchedError>> {
        info!("changing log level to {}", level);
        log::set_max_level(level);
        Reply::Success.write_to(stream)?;
        Ok(())
    }

    pub fn set_uart_log_filter(_io: &Io, stream: &mut TcpStream, level: LevelFilter) -> Result<(), Error<SchedError>> {
        info!("changing UART log level to {}", level);
        BufferLogger::with(|logger|
            logger.set_uart_log_level(level));
        Reply::Success.write_to(stream)?;
        Ok(())
    }

    pub fn config_read(_io: &Io, stream: &mut TcpStream, key: &String) -> Result<(), Error<SchedError>>{
        config::read(key, |result| {
            match result {
                Ok(value) => Reply::ConfigData(&value).write_to(stream),
                Err(_)    => Reply::Error.write_to(stream)
            }
        })?;
        Ok(())
    }

    pub fn config_write(_io: &Io, stream: &mut TcpStream, key: &String, value: &Vec<u8>, restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        match config::write(key, value) {
            Ok(_) => {
                if key == "idle_kernel" {
                    io.until(|| !restart_idle.get())?;
                    restart_idle.set(true);
                }
                Reply::Success.write_to(stream)
            },
            Err(_) => Reply::Error.write_to(stream)
        }?;
        Ok(())
    }

    pub fn config_remove(_io: &Io, stream: &mut TcpStream, key: &String, restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        match config::remove(key) {
            Ok(()) => {
                if key == "idle_kernel" {
                    io.until(|| !restart_idle.get())?;
                    restart_idle.set(true);
                }
                Reply::Success.write_to(stream)
            },
            Err(_) => Reply::Error.write_to(stream)
        }?;
        Ok(())
    }

    pub fn config_erase(_io: &Io, stream: &mut TcpStream, restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        match config::erase() {
            Ok(()) => {
                io.until(|| !restart_idle.get())?;
                restart_idle.set(true);
                Reply::Success.write_to(stream)
            },
            Err(_) => Reply::Error.write_to(stream)
        }?;
        Ok(())
    }

    pub fn reboot(_io: &Io, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        Reply::RebootImminent.write_to(stream)?;
        stream.close()?;
        stream.flush()?;

        warn!("restarting");
        unsafe { spiflash::reload(); }
    }

    pub fn debug_allocator(_io: &Io, _stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        unsafe { println!("{}", ::ALLOC) }
        Ok(())
    }
}

#[cfg(has_drtio)]
mod remote_coremgmt {
    use alloc::{string::String, vec::Vec};
    use log::LevelFilter;

    use board_artiq::{drtioaux::Packet, drtio_routing};
    use io::{Cursor, ProtoWrite};
    use mgmt_proto::{Error, Reply};
    use rtio_mgt::drtio;
    use sched::{Io, Mutex, TcpStream, Error as SchedError};
    use proto_artiq::drtioaux_proto::MASTER_PAYLOAD_MAX_SIZE;


    impl From<drtio::Error> for Error<SchedError> {
        fn from(_value: drtio::Error) -> Error<SchedError> {
            Error::DrtioError
        }
    }

    pub fn get_log(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        let mut buffer = String::new();
        loop {
            let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
                &Packet::CoreMgmtGetLogRequest { destination, clear: false }
            );
            
            match reply {
                Ok(Packet::CoreMgmtGetLogReply { last, length, data }) => {
                    buffer.push_str(
                        core::str::from_utf8(&data[..length as usize]).map_err(|_| Error::DrtioError)?);
                    if last {
                        Reply::LogContent(&buffer).write_to(stream)?;
                        return Ok(());
                    }
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    Reply::Error.write_to(stream)?;
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    Reply::Error.write_to(stream)?;
                    return Err(e.into());
                }
            }
        }
    }

    pub fn clear_log(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtClearLogRequest { destination }
        );

        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn pull_log(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        loop {
            let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
                &Packet::CoreMgmtGetLogRequest { destination, clear: true }
            );

            match reply {
                Ok(Packet::CoreMgmtGetLogReply { last: _, length, data }) => {
                    stream.write_bytes(&data[..length as usize])?;
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    return Err(e.into());
                }
            }
        }
    }

    pub fn set_log_filter(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, level: LevelFilter) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtSetLogLevelRequest { destination, log_level: level as u8 }
        );

        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn set_uart_log_filter(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, level: LevelFilter) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtSetUartLogLevelRequest { destination, log_level: level as u8 }
        );

        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn config_read(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, key: &String) -> Result<(), Error<SchedError>> {
        let mut config_key: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
        let len = key.len();
        config_key[..len].clone_from_slice(key.as_bytes());

        let mut reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtConfigReadRequest {
                destination: destination,
                length: len as u16,
                key: config_key,
            }
        );

        let mut buffer = Vec::<u8>::new();
        loop {
            match reply {
                Ok(Packet::CoreMgmtConfigReadReply { length, last, value }) => {
                    buffer.extend(&value[..length as usize]);

                    if last {
                        Reply::ConfigData(&buffer).write_to(stream)?;
                        return Ok(());
                    }

                    reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
                        &Packet::CoreMgmtConfigReadContinue {
                            destination: destination,
                        }
                    );
                }
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    Reply::Error.write_to(stream)?;
                    return Err(drtio::Error::UnexpectedReply.into());
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    Reply::Error.write_to(stream)?;
                    return Err(e.into());
                }
            }
        }
    }

    pub fn config_write(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, key: &String, value: &Vec<u8>,
        _restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        let mut message = Cursor::new(Vec::with_capacity(key.len() + value.len() + 4 * 2));
        message.write_string(key).unwrap();
        message.write_bytes(value).unwrap();

        match drtio::partition_data(message.get_ref(), |slice, status, len: usize| {
            let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno, 
                &Packet::CoreMgmtConfigWriteRequest {
                    destination: destination, length: len as u16, last: status.is_last(), data: *slice});
            match reply {
                Ok(Packet::CoreMgmtAck) => Ok(()),
                Ok(packet) => {
                    error!("received unexpected aux packet: {:?}", packet);
                    Err(drtio::Error::UnexpectedReply)
                }
                Err(e) => {
                    error!("aux packet error ({})", e);
                    Err(e)
                }
            }
        }) {
            Ok(()) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            },
            Err(e) => {
                Reply::Error.write_to(stream)?;
                Err(e.into())
            },
        }
    }

    pub fn config_remove(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, key: &String,
        _restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        let mut config_key: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
        let len = key.len();
        config_key[..len].clone_from_slice(key.as_bytes());

        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtConfigRemoveRequest {
                destination: destination,
                length: key.len() as u16,
                key: config_key,
            });

        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn config_erase(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream, _restart_idle: &Urc<Cell<bool>>) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtConfigEraseRequest {
                destination: destination,
            });
        
        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::Success.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn reboot(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtRebootRequest {
                destination: destination,
            });
        
        match reply {
            Ok(Packet::CoreMgmtAck) => {
                Reply::RebootImminent.write_to(stream)?;
                Ok(())
            }
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Reply::Error.write_to(stream)?;
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Reply::Error.write_to(stream)?;
                Err(e.into())
            }
        }
    }

    pub fn debug_allocator(io: &Io, aux_mutex: &Mutex,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex, 
        routing_table: &drtio_routing::RoutingTable, linkno: u8,
        destination: u8, _stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
        let reply = drtio::aux_transact(io, aux_mutex, ddma_mutex, subkernel_mutex, routing_table, linkno,
            &Packet::CoreMgmtAllocatorDebugRequest {
                destination: destination,
            });

        match reply {
            Ok(Packet::CoreMgmtAck) => Ok(()),
            Ok(packet) => {
                error!("received unexpected aux packet: {:?}", packet);
                Err(drtio::Error::UnexpectedReply.into())
            }
            Err(e) => {
                error!("aux packet error ({})", e);
                Err(e.into())
            }
        }
    }
}

#[cfg(has_drtio)]
macro_rules! process {
    ($io:ident, $aux_mutex:ident, $ddma_mutex:ident, $subkernel_mutex:ident, $routing_table:ident, $tcp_stream:ident, $destination: ident, $func:ident $(, $param:expr)*) => {{
        let hop = $routing_table.0[$destination as usize][0];
        if hop == 0 {
            local_coremgmt::$func($io, $tcp_stream, $($param, )*)
        } else {
            let linkno = hop - 1;
            remote_coremgmt::$func($io, $aux_mutex, $ddma_mutex, $subkernel_mutex, $routing_table, linkno, $destination, $tcp_stream, $($param, )*)
        }
    }}
}

#[cfg(not(has_drtio))]
macro_rules! process {
    ($io:ident, $aux_mutex:ident, $ddma_mutex:ident, $subkernel_mutex:ident, $routing_table:ident, $tcp_stream:ident, $_destination: ident, $func:ident $(, $param:expr)*) => {{
        local_coremgmt::$func($io, $tcp_stream, $($param, )*)
    }}
}

fn worker(io: &Io, stream: &mut TcpStream, restart_idle: &Urc<Cell<bool>>,
    _aux_mutex: &Mutex, _ddma_mutex: &Mutex, _subkernel_mutex: &Mutex,
    _routing_table: &drtio_routing::RoutingTable, stream: &mut TcpStream) -> Result<(), Error<SchedError>> {
    read_magic(stream)?;
    let _destination = stream.read_u8()?;
    Write::write_all(stream, "e".as_bytes())?;
    info!("new connection from {}", stream.remote_endpoint());

    loop {
        match Request::read_from(stream)? {
            Request::GetLog => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, get_log),
            Request::ClearLog => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, clear_log),
            Request::PullLog => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, pull_log),
            Request::SetLogFilter(level) => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, set_log_filter, level),
            Request::SetUartLogFilter(level) => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, set_uart_log_filter, level),
            Request::ConfigRead { ref key } => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, config_read, key),
            Request::ConfigWrite { ref key, ref value } => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, config_write, key, value, restart_idle),
            Request::ConfigRemove { ref key } => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, config_remove, key, restart_idle),
            Request::ConfigErase => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, config_erase, restart_idle),
            Request::Reboot => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, reboot),
            Request::DebugAllocator => process!(io, _aux_mutex, _ddma_mutex, _subkernel_mutex, _routing_table, stream, _destination, debug_allocator),
        }?;
    }
}

pub fn thread(io: Io, restart_idle: &Urc<Cell<bool>>, aux_mutex: &Mutex, ddma_mutex: &Mutex, subkernel_mutex: &Mutex, routing_table: &Urc<RefCell<drtio_routing::RoutingTable>>) {
    let listener = TcpListener::new(&io, 8192);
    listener.listen(1380).expect("mgmt: cannot listen");
    info!("management interface active");

    loop {
        let restart_idle = restart_idle.clone();
        let aux_mutex = aux_mutex.clone();
        let ddma_mutex = ddma_mutex.clone();
        let subkernel_mutex = subkernel_mutex.clone();
        let routing_table = routing_table.clone();
        let stream = listener.accept().expect("mgmt: cannot accept").into_handle();
        io.spawn(16384, move |io| {
            let routing_table = routing_table.borrow();
            let mut stream = TcpStream::from_handle(&io, stream);
            match worker(&io, &mut stream, &restart_idle, &aux_mutex, &ddma_mutex, &subkernel_mutex, &routing_table) {
                Ok(()) => (),
                Err(Error::Io(IoError::UnexpectedEnd)) => (),
                Err(err) => error!("aborted: {}", err)
            }
            stream.close().expect("mgmt: close socket");
        });
    }
}
