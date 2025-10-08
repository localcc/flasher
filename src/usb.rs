use embassy_usb::{
    class::cdc_acm::{Receiver, Sender},
    driver::{Driver, EndpointError},
};

use crate::stream::circular_bytes::CircularBytes;

const MAX_FULL_SPEED_PACKET_SIZE: usize = 64;

pub struct UsbReadStream<'a, 'd, D: Driver<'d>> {
    receiver: &'a mut Receiver<'d, D>,
    pending_read: CircularBytes<MAX_FULL_SPEED_PACKET_SIZE>,
}

impl<'a, 'd, D: Driver<'d>> UsbReadStream<'a, 'd, D> {
    pub fn new(receiver: &'a mut Receiver<'d, D>) -> Self {
        UsbReadStream {
            receiver,
            pending_read: CircularBytes::new(),
        }
    }
}

impl<'a, 'd, D: Driver<'d>> embedded_io_async::Read for UsbReadStream<'a, 'd, D> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if !self.pending_read.is_empty() {
            return Ok(self.pending_read.read(buf).await.unwrap());
        }

        let mut usb_buf = [0u8; MAX_FULL_SPEED_PACKET_SIZE];

        // doing a second read in case of a ZLP here
        for _ in 0..2 {
            let read = self.receiver.read_packet(&mut usb_buf).await?;
            if read != 0 {
                let usb_buf = &usb_buf[..read];
                self.pending_read.extend_from_slice(&usb_buf);
                break;
            }
        }

        let read = self.pending_read.read(buf).await.unwrap();
        Ok(read)
    }
}

impl<'a, 'd, D: Driver<'d>> embedded_io_async::ErrorType for UsbReadStream<'a, 'd, D> {
    type Error = EndpointError;
}

pub struct UsbWriteStream<'a, 'd, D: Driver<'d>> {
    sender: &'a mut Sender<'d, D>,
}

impl<'a, 'd, D: Driver<'d>> UsbWriteStream<'a, 'd, D> {
    pub fn new(sender: &'a mut Sender<'d, D>) -> Self {
        UsbWriteStream { sender }
    }
}

impl<'a, 'd, D: Driver<'d>> embedded_io_async::Write for UsbWriteStream<'a, 'd, D> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let len = buf.len().min(MAX_FULL_SPEED_PACKET_SIZE);
        let written = &buf[..len];
        self.sender.write_packet(&written).await?;
        Ok(len)
    }
}

impl<'a, 'd, D: Driver<'d>> embedded_io_async::ErrorType for UsbWriteStream<'a, 'd, D> {
    type Error = EndpointError;
}
