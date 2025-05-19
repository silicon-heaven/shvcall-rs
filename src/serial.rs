use std::time::Duration;
use smol::Unblock;
use shvrpc::framerw::FrameWriter;
use shvrpc::framerw::FrameReader;
use shvrpc::serialrw::{SerialFrameReader, SerialFrameWriter};
use log::info;
use serialport::SerialPort;
use futures::io::{BufReader, BufWriter};

fn open_serial(port_name: &str) -> shvrpc::Result<(Box<dyn SerialPort>, Box<dyn SerialPort>)> {
    info!("Opening serial port: {}", port_name);
    let port = serialport::new(port_name, 115200)
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        // serial port should never timeout,
        // timeout on serial breaks reader loop
        .timeout(Duration::from_secs(60 * 60 *24 * 365 * 100))
        .open()?;

    // Clone the port
    let port_clone = port.try_clone()?;
    info!("open serial port OK");
    Ok((port, port_clone))
}

pub(crate) fn create_serial_frame_reader_writer(port_name: &str) -> shvrpc::Result<(impl FrameReader + use<>, impl FrameWriter + use<>)> {
    let (rd, wr) = open_serial(port_name)?;
    let serial_reader = Unblock::new(rd);
    let serial_writer = Unblock::new(wr);

    let brd = BufReader::new(serial_reader);
    let bwr = BufWriter::new(serial_writer);

    let frame_reader = SerialFrameReader::new(brd).with_crc_check(true);
    let frame_writer = SerialFrameWriter::new(bwr).with_crc_check(true);

    Ok((frame_reader, frame_writer))
}

