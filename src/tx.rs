use core::ops::{Deref, DerefMut};

#[cfg(feature = "stm32f107")]
use stm32f1::stm32f107::ETHERNET_DMA;
#[cfg(feature = "stm32f4xx")]
use stm32f4xx_hal::stm32::ETHERNET_DMA;

use crate::{
    desc::Descriptor,
    ring::{RingDescriptor, RingEntry},
};

/// Owned by DMA engine
const TXDESC_0_OWN: u32 = 1 << 31;
/// Interrupt on completion
const TXDESC_0_IC: u32 = 1 << 30;
/// First segment of frame
const TXDESC_0_FS: u32 = 1 << 28;
/// Last segment of frame
const TXDESC_0_LS: u32 = 1 << 29;
/// Transmit end of ring
const TXDESC_0_TER: u32 = 1 << 21;
/// Second address chained
const TXDESC_0_TCH: u32 = 1 << 20;
/// Error status
const TXDESC_0_ES: u32 = 1 << 15;

const TXDESC_1_TBS_SHIFT: usize = 0;
const TXDESC_1_TBS_MASK: u32 = 0x0fff << TXDESC_1_TBS_SHIFT;

#[derive(Debug, PartialEq)]
pub enum TxError {
    /// Ring buffer is full
    WouldBlock,
}

#[repr(C)]
#[derive(Clone)]
pub struct TxDescriptor {
    desc: Descriptor,
}

impl Default for TxDescriptor {
    fn default() -> Self {
        let mut desc = Descriptor::default();
        unsafe {
            desc.write(0, TXDESC_0_TCH | TXDESC_0_IC | TXDESC_0_FS | TXDESC_0_LS);
        }
        TxDescriptor { desc }
    }
}

impl TxDescriptor {
    /// Is owned by the DMA engine?
    fn is_owned(&self) -> bool {
        (self.desc.read(0) & TXDESC_0_OWN) == TXDESC_0_OWN
    }

    /// Pass ownership to the DMA engine
    fn set_owned(&mut self) {
        unsafe {
            self.desc.modify(0, |w| w | TXDESC_0_OWN);
        }
    }

    #[allow(unused)]
    fn has_error(&self) -> bool {
        (self.desc.read(0) & TXDESC_0_ES) == TXDESC_0_ES
    }

    fn set_buffer1(&mut self, buffer: *const u8) {
        unsafe {
            self.desc.write(2, buffer as u32);
        }
    }

    fn set_buffer1_len(&mut self, len: usize) {
        unsafe {
            self.desc.modify(1, |w| {
                (w & !TXDESC_1_TBS_MASK) | ((len as u32) << TXDESC_1_TBS_SHIFT)
            });
        }
    }

    // points to next descriptor (RCH)
    fn set_buffer2(&mut self, buffer: *const u8) {
        unsafe {
            self.desc.write(3, buffer as u32);
        }
    }

    fn set_end_of_ring(&mut self) {
        unsafe {
            self.desc.modify(0, |w| w | TXDESC_0_TER);
        }
    }
}

pub type TxRingEntry = RingEntry<TxDescriptor>;

impl RingDescriptor for TxDescriptor {
    fn setup(&mut self, buffer: *const u8, _len: usize, next: Option<&Self>) {
        self.set_buffer1(buffer);
        match next {
            Some(next) => self.set_buffer2(&next.desc as *const Descriptor as *const u8),
            None => {
                self.set_buffer2(0 as *const u8);
                self.set_end_of_ring();
            }
        };
    }
}

impl TxRingEntry {
    fn prepare_packet<'a>(&'a mut self, length: usize) -> Option<TxPacket<'a>> {
        assert!(length <= self.as_slice().len());

        if !self.desc().is_owned() {
            self.desc_mut().set_buffer1_len(length);
            Some(TxPacket {
                entry: self,
                length,
            })
        } else {
            None
        }
    }
}

pub struct TxPacket<'a> {
    entry: &'a mut TxRingEntry,
    length: usize,
}

impl<'a> Deref for TxPacket<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.entry.as_slice()[0..self.length]
    }
}

impl<'a> DerefMut for TxPacket<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry.as_mut_slice()[0..self.length]
    }
}

impl<'a> TxPacket<'a> {
    // Pass to DMA engine
    pub fn send(self) {
        self.entry.desc_mut().set_owned();
    }
}

/// Tx DMA state
pub struct TxRing<'a> {
    entries: &'a mut [TxRingEntry],
    next_entry: usize,
}

impl<'a> TxRing<'a> {
    /// Allocate
    ///
    /// `start()` will be needed before `send()`
    pub fn new(entries: &'a mut [TxRingEntry]) -> Self {
        TxRing {
            entries,
            next_entry: 0,
        }
    }

    /// Start the Tx DMA engine
    pub fn start(&mut self, eth_dma: &ETHERNET_DMA) {
        // Setup ring
        {
            let mut previous: Option<&mut TxRingEntry> = None;
            for entry in self.entries.iter_mut() {
                previous.map(|previous| previous.setup(Some(entry)));
                previous = Some(entry);
            }
            previous.map(|previous| previous.setup(None));
        }

        let ring_ptr = self.entries[0].desc() as *const TxDescriptor;
        // Register TxDescriptor
        eth_dma.dmatdlar.write(|w| w.stl().bits(ring_ptr as u32));

        // Start transmission
        eth_dma.dmaomr.modify(|_, w| {
            w.st()
                .set_bit()
                .ttc()
                .ttc16()
                .tsf()
                .clear_bit()
                .fef()
                .set_bit()
                .fugf()
                .set_bit()
                .osf()
                .set_bit()
        });
    }

    pub fn send<F: FnOnce(&mut [u8]) -> R, R>(
        &mut self,
        length: usize,
        f: F,
    ) -> Result<R, TxError> {
        let entries_len = self.entries.len();

        match self.entries[self.next_entry].prepare_packet(length) {
            Some(mut pkt) => {
                let r = f(pkt.deref_mut());
                pkt.send();

                self.next_entry += 1;
                if self.next_entry >= entries_len {
                    self.next_entry = 0;
                }
                Ok(r)
            }
            None => Err(TxError::WouldBlock),
        }
    }

    /// Demand that the DMA engine polls the current `TxDescriptor`
    /// (when we just transferred ownership to the hardware).
    pub fn demand_poll(&self, eth_dma: &ETHERNET_DMA) {
        eth_dma.dmatpdr.write(|w| w.tpd().poll());
    }

    /// Is the Tx DMA engine running?
    pub fn is_running(&self, eth_dma: &ETHERNET_DMA) -> bool {
        self.running_state(&eth_dma).is_running()
    }

    fn running_state(&self, eth_dma: &ETHERNET_DMA) -> RunningState {
        match eth_dma.dmasr.read().tps().bits() {
            // Reset or Stop Transmit Command issued
            0b000 => RunningState::Stopped,
            // Fetching transmit transfer descriptor
            0b001 => RunningState::Running,
            // Waiting for status
            0b010 => RunningState::Running,
            // Reading Data from host memory buffer and queuing it to transmit buffer
            0b011 => RunningState::Running,
            0b100 | 0b101 => RunningState::Reserved,
            // Transmit descriptor unavailable
            0b110 => RunningState::Suspended,
            _ => RunningState::Unknown,
        }
    }
}

#[derive(Debug, PartialEq)]
enum RunningState {
    /// Reset or Stop Transmit Command issued
    Stopped,
    /// Fetching transmit transfer descriptor;
    /// Waiting for status;
    /// Reading Data from host memory buffer and queuing it to transmit buffer
    Running,
    /// Reserved for future use
    Reserved,
    /// Transmit descriptor unavailable
    Suspended,
    /// Invalid value
    Unknown,
}

impl RunningState {
    pub fn is_running(&self) -> bool {
        *self == RunningState::Running
    }
}
