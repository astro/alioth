// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use macros::Layout;
use mio::Waker;
use parking_lot::{Mutex, RwLock};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::hv::MsiSender;
use crate::mem::emulated::Mmio;
use crate::mem::{MemRange, MemRegion, MemRegionEntry};
use crate::pci::cap::{
    MsixCap, MsixCapMmio, MsixCapOffset, MsixMsgCtrl, MsixTableEntry, MsixTableMmio, PciCap,
    PciCapHdr, PciCapId, PciCapList,
};
use crate::pci::config::{
    CommonHeader, DeviceHeader, EmulatedConfig, HeaderType, PciConfig, BAR_MEM64, BAR_PREFETCHABLE,
};
use crate::pci::{self, Pci, PciBar};
use crate::utils::{
    get_atomic_high32, get_atomic_low32, get_high32, get_low32, set_atomic_high32, set_atomic_low32,
};
use crate::virtio::dev::{Register, WakeEvent};
use crate::virtio::queue::Queue;
use crate::virtio::{DevStatus, IrqSender};
use crate::{impl_mmio_for_zerocopy, mem};

use super::dev::{Virtio, VirtioDevice};
use super::{DeviceId, Result};

const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;

#[derive(Debug)]
struct VirtioPciMsixVector {
    config: AtomicU16,
    queues: Vec<AtomicU16>,
}

#[derive(Debug)]
pub struct PciIrqSender<S> {
    msix_vector: VirtioPciMsixVector,
    msix_entries: Arc<Vec<RwLock<MsixTableEntry>>>,
    msi_sender: S,
}

impl<S> PciIrqSender<S>
where
    S: MsiSender,
{
    fn send(&self, vector: u16) {
        let entries = &**self.msix_entries;
        let Some(entry) = entries.get(vector as usize) else {
            log::error!("invalid config vector: {:x}", vector);
            return;
        };
        let entry = entry.read();
        if entry.control.masked() {
            log::info!("{} is masked", vector);
            return;
        }
        let data = entry.data;
        let addr = ((entry.addr_hi as u64) << 32) | (entry.addr_lo as u64);
        if let Err(e) = self.msi_sender.send(addr, data) {
            log::error!("send msi data = {data:#x} to {addr:#x}: {e}")
        } else {
            log::trace!("send msi data = {data:#x} to {addr:#x}: done")
        }
    }
}

impl<S> IrqSender for PciIrqSender<S>
where
    S: MsiSender,
{
    fn config_irq(&self) {
        let vector = self.msix_vector.config.load(Ordering::Acquire);
        if vector != VIRTIO_MSI_NO_VECTOR {
            self.send(vector)
        }
    }

    fn queue_irq(&self, idx: u16) {
        let Some(vector) = self.msix_vector.queues.get(idx as usize) else {
            log::error!("invalid queue index: {idx}");
            return;
        };
        let vector = vector.load(Ordering::Acquire);
        if vector != VIRTIO_MSI_NO_VECTOR {
            self.send(vector);
        }
    }
}

#[repr(C, align(4))]
#[derive(Layout)]
pub struct VirtioCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    config_msix_vector: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc_lo: u32,
    queue_desc_hi: u32,
    queue_driver_lo: u32,
    queue_driver_hi: u32,
    queue_device_lo: u32,
    queue_device_hi: u32,
    queue_notify_data: u16,
    queue_reset: u16,
}

#[derive(Layout)]
#[repr(C, align(4))]
pub struct VirtioPciRegister {
    common: VirtioCommonCfg,
    isr_status: u32,
    queue_notify: PhantomData<[u32]>,
}

#[derive(Debug)]
pub struct VirtioPciRegisterMmio<M>
where
    M: MsiSender,
{
    name: Arc<String>,
    reg: Arc<Register>,
    queues: Arc<Vec<Queue>>,
    irq_sender: Arc<PciIrqSender<M>>,
    event_tx: Sender<WakeEvent<PciIrqSender<M>>>,
    waker: Arc<Waker>,
}

impl<M> VirtioPciRegisterMmio<M>
where
    M: MsiSender,
{
    fn wake_up_dev(&self, event: WakeEvent<PciIrqSender<M>>) {
        if let Err(e) = self.event_tx.send(event) {
            log::error!("{}: failed to send event: {e}", self.name);
            return;
        }
        if let Err(e) = self.waker.wake() {
            log::error!("{}: failed to wake up device: {e}", self.name);
        }
    }

    fn reset(&self) {
        let config_msix = &self.irq_sender.msix_vector.config;
        config_msix.store(VIRTIO_MSI_NO_VECTOR, Ordering::Release);
        for q_vector in self.irq_sender.msix_vector.queues.iter() {
            q_vector.store(VIRTIO_MSI_NO_VECTOR, Ordering::Release);
        }
        for q in self.queues.iter() {
            q.enabled.store(false, Ordering::Release);
        }
    }
}

impl<M> Mmio for VirtioPciRegisterMmio<M>
where
    M: MsiSender,
{
    fn size(&self) -> usize {
        size_of::<VirtioPciRegister>() + size_of::<u32>() * self.queues.len()
    }

    fn read(&self, offset: usize, size: u8) -> mem::Result<u64> {
        let reg = &*self.reg;
        let ret = match (offset, size as usize) {
            VirtioCommonCfg::LAYOUT_DEVICE_FEATURE_SELECT => {
                reg.device_feature_sel.load(Ordering::Acquire) as u64
            }
            VirtioCommonCfg::LAYOUT_DEVICE_FEATURE => {
                if reg.device_feature_sel.load(Ordering::Acquire) > 0 {
                    get_high32(reg.device_feature) as u64
                } else {
                    get_low32(reg.device_feature) as u64
                }
            }
            VirtioCommonCfg::LAYOUT_DRIVER_FEATURE_SELECT => {
                reg.driver_feature_sel.load(Ordering::Acquire) as u64
            }
            VirtioCommonCfg::LAYOUT_DRIVER_FEATURE => {
                if reg.driver_feature_sel.load(Ordering::Acquire) > 0 {
                    get_atomic_high32(&reg.driver_feature) as u64
                } else {
                    get_atomic_low32(&reg.driver_feature) as u64
                }
            }
            VirtioCommonCfg::LAYOUT_CONFIG_MSIX_VECTOR => {
                self.irq_sender.msix_vector.config.load(Ordering::Acquire) as u64
            }
            VirtioCommonCfg::LAYOUT_NUM_QUEUES => self.queues.len() as u64,
            VirtioCommonCfg::LAYOUT_DEVICE_STATUS => reg.status.load(Ordering::Acquire) as u64,
            VirtioCommonCfg::LAYOUT_CONFIG_GENERATION => {
                0 // TODO: support device config change at runtime
            }
            VirtioCommonCfg::LAYOUT_QUEUE_SELECT => reg.queue_sel.load(Ordering::Acquire) as u64,
            VirtioCommonCfg::LAYOUT_QUEUE_SIZE => {
                let q_sel = reg.queue_sel.load(Ordering::Acquire) as usize;
                if let Some(q) = self.queues.get(q_sel) {
                    q.size.load(Ordering::Acquire) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_MSIX_VECTOR => {
                let q_sel = reg.queue_sel.load(Ordering::Acquire) as usize;
                if let Some(msix_vector) = self.irq_sender.msix_vector.queues.get(q_sel) {
                    msix_vector.load(Ordering::Acquire) as u64
                } else {
                    VIRTIO_MSI_NO_VECTOR as u64
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_ENABLE => {
                let q_sel = reg.queue_sel.load(Ordering::Acquire) as usize;
                if let Some(q) = self.queues.get(q_sel) {
                    q.enabled.load(Ordering::Acquire) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_NOTIFY_OFF => {
                reg.queue_sel.load(Ordering::Acquire) as u64
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DESC_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_low32(&q.desc) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DESC_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_high32(&q.desc) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DRIVER_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_high32(&q.driver) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DRIVER_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_high32(&q.driver) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DEVICE_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_high32(&q.device) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DEVICE_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    get_atomic_high32(&q.device) as u64
                } else {
                    0
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_NOTIFY_DATA => {
                todo!()
            }
            VirtioCommonCfg::LAYOUT_QUEUE_RESET => {
                todo!()
            }
            _ => {
                log::error!(
                    "{}: read invalid register: offset = {offset:#x}, size = {size}",
                    self.name
                );
                0
            }
        };
        Ok(ret)
    }

    fn write(&self, offset: usize, size: u8, val: u64) -> mem::Result<()> {
        let reg = &*self.reg;
        match (offset, size as usize) {
            VirtioCommonCfg::LAYOUT_DEVICE_FEATURE_SELECT => {
                reg.device_feature_sel.store(val as u8, Ordering::Release);
            }
            VirtioCommonCfg::LAYOUT_DRIVER_FEATURE_SELECT => {
                reg.driver_feature_sel.store(val as u8, Ordering::Release);
            }
            VirtioCommonCfg::LAYOUT_DRIVER_FEATURE => {
                if reg.driver_feature_sel.load(Ordering::Relaxed) > 0 {
                    set_atomic_high32(&reg.driver_feature, val as u32)
                } else {
                    set_atomic_low32(&reg.driver_feature, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_CONFIG_MSIX_VECTOR => {
                let config_msix = &self.irq_sender.msix_vector.config;
                let old = config_msix.swap(val as u16, Ordering::AcqRel);
                log::trace!(
                    "{}: config MSI-X vector update: {old:#x} -> {val:#x}",
                    self.name
                );
            }
            VirtioCommonCfg::LAYOUT_DEVICE_STATUS => {
                let status = DevStatus::from_bits_truncate(val as u8);
                let old = reg.status.swap(status.bits(), Ordering::AcqRel);
                let old = DevStatus::from_bits_retain(old);
                if (old ^ status).contains(DevStatus::DRIVER_OK) {
                    let event = if status.contains(DevStatus::DRIVER_OK) {
                        WakeEvent::Start {
                            feature: reg.driver_feature.load(Ordering::Acquire),
                            irq_sender: self.irq_sender.clone(),
                        }
                    } else {
                        self.reset();
                        WakeEvent::Reset
                    };
                    self.wake_up_dev(event);
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_SELECT => {
                reg.queue_sel.store(val as u16, Ordering::Relaxed);
                if self.queues.get(val as usize).is_none() {
                    log::error!("{}: unknown queue index {val}", self.name)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_SIZE => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed) as usize;
                if let Some(q) = self.queues.get(q_sel) {
                    // TODO: validate queue size
                    q.size.store(val as u16, Ordering::Release);
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_MSIX_VECTOR => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed) as usize;
                if let Some(msix_vector) = self.irq_sender.msix_vector.queues.get(q_sel) {
                    let old = msix_vector.swap(val as u16, Ordering::AcqRel);
                    log::trace!(
                        "{}: queue {q_sel} MSI-X vector update: {old:#x} -> {val:#x}",
                        self.name
                    );
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_ENABLE => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    q.enabled.store(val != 0, Ordering::Release);
                };
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DESC_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_low32(&q.desc, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DESC_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_high32(&q.desc, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DRIVER_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_low32(&q.driver, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DRIVER_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_high32(&q.driver, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DEVICE_LO => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_low32(&q.device, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_DEVICE_HI => {
                let q_sel = reg.queue_sel.load(Ordering::Relaxed);
                if let Some(q) = self.queues.get(q_sel as usize) {
                    set_atomic_high32(&q.device, val as u32)
                }
            }
            VirtioCommonCfg::LAYOUT_QUEUE_RESET => {
                todo!()
            }
            (VirtioPciRegister::OFFSET_QUEUE_NOTIFY, _) => {
                todo!()
            }
            _ => {
                log::error!(
                    "{}: write 0x{val:0width$x} to invalid register offset = {offset:#x}",
                    self.name,
                    width = 2 * size as usize
                );
            }
        }
        Ok(())
    }
}

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_DEVICE_ID_BASE: u16 = 0x1040;

fn get_class(id: DeviceId) -> (u8, u8) {
    match id {
        DeviceId::Net => (0x02, 0x00),
        DeviceId::FileSystem => (0x01, 0x80),
        DeviceId::Block => (0x01, 0x00),
        DeviceId::Socket => (0x02, 0x80),
        _ => (0xff, 0x00),
    }
}

#[repr(u8)]
pub enum VirtioPciCfg {
    Common = 1,
    Notify = 2,
    Isr = 3,
    Device = 4,
    Pci = 5,
    SharedMemory = 8,
    Vendor = 9,
}

#[repr(C, align(4))]
#[derive(Debug, Default, FromBytes, FromZeroes, AsBytes)]
pub struct VirtioPciCap {
    header: PciCapHdr,
    cap_len: u8,
    cfg_type: u8,
    bar: u8,
    id: u8,
    padding: [u8; 2],
    offset: u32,
    length: u32,
}
impl_mmio_for_zerocopy!(VirtioPciCap);

impl PciCap for VirtioPciCap {
    fn set_next(&mut self, val: u8) {
        self.header.next = val
    }
}

#[repr(C, align(4))]
#[derive(Debug, Default, FromBytes, FromZeroes, AsBytes)]
pub struct VirtioPciCap64 {
    cap: VirtioPciCap,
    offset_hi: u32,
    length_hi: u32,
}
impl_mmio_for_zerocopy!(VirtioPciCap64);

impl PciCap for VirtioPciCap64 {
    fn set_next(&mut self, val: u8) {
        PciCap::set_next(&mut self.cap, val)
    }
}

#[repr(C, align(4))]
#[derive(Debug, Default, FromBytes, FromZeroes, AsBytes)]
pub struct VirtioPciNotifyCap {
    cap: VirtioPciCap,
    multiplier: u32,
}
impl_mmio_for_zerocopy!(VirtioPciNotifyCap);

impl PciCap for VirtioPciNotifyCap {
    fn set_next(&mut self, val: u8) {
        self.cap.header.next = val;
    }
}

#[derive(Debug)]
pub struct VirtioPciDevice<D, M>
where
    D: Virtio,
    M: MsiSender,
{
    pub dev: VirtioDevice<D, PciIrqSender<M>>,
    pub config: Arc<EmulatedConfig>,
    pub registers: Arc<VirtioPciRegisterMmio<M>>,
}

impl<D, M> VirtioPciDevice<D, M>
where
    M: MsiSender,
    D: Virtio,
{
    pub fn new(dev: VirtioDevice<D, PciIrqSender<M>>, msi_sender: M) -> Result<Self> {
        let (class, subclass) = get_class(D::device_id());
        let mut header = DeviceHeader {
            common: CommonHeader {
                vendor: VIRTIO_VENDOR_ID,
                device: VIRTIO_DEVICE_ID_BASE + D::device_id() as u16,
                revision: 0x1,
                header_type: HeaderType::Device as u8,
                class,
                subclass,
                ..Default::default()
            },
            subsystem: VIRTIO_DEVICE_ID_BASE + D::device_id() as u16,
            ..Default::default()
        };
        let device_config = dev.device_config.clone();
        let num_queues = dev.queue_regs.len();
        let table_entries = num_queues + 1;

        let msix_table_offset = 0;
        let msix_table_size = size_of::<MsixTableEntry>() * table_entries;

        let msix_pba_offset = 8 << 10;

        let virtio_register_offset = 12 << 10;
        let device_config_offset =
            virtio_register_offset + size_of::<VirtioPciRegister>() + size_of::<u32>() * num_queues;

        let msix_msg_ctrl = MsixMsgCtrl::new(table_entries as u16);

        let cap_msix = MsixCap {
            header: PciCapHdr {
                id: PciCapId::Msix as u8,
                ..Default::default()
            },
            control: msix_msg_ctrl,
            table_offset: MsixCapOffset(msix_table_offset as u32),
            pba_offset: MsixCapOffset(msix_pba_offset as u32),
        };
        let cap_common = VirtioPciCap {
            header: PciCapHdr {
                id: PciCapId::Vendor as u8,
                ..Default::default()
            },
            cap_len: size_of::<VirtioPciCap>() as u8,
            cfg_type: VirtioPciCfg::Common as u8,
            bar: 0,
            id: 0,
            offset: (virtio_register_offset + VirtioPciRegister::OFFSET_COMMON) as u32,
            length: size_of::<VirtioCommonCfg>() as u32,
            ..Default::default()
        };
        let cap_isr = VirtioPciCap {
            header: PciCapHdr {
                id: PciCapId::Vendor as u8,
                ..Default::default()
            },
            cap_len: size_of::<VirtioPciCap>() as u8,
            cfg_type: VirtioPciCfg::Isr as u8,
            bar: 0,
            id: 0,
            offset: (virtio_register_offset + VirtioPciRegister::OFFSET_ISR_STATUS) as u32,
            length: size_of::<u32>() as u32,
            ..Default::default()
        };
        let cap_notify = VirtioPciNotifyCap {
            cap: VirtioPciCap {
                header: PciCapHdr {
                    id: PciCapId::Vendor as u8,
                    ..Default::default()
                },
                cap_len: size_of::<VirtioPciNotifyCap>() as u8,
                cfg_type: VirtioPciCfg::Notify as u8,
                bar: 0,
                id: 0,
                offset: (virtio_register_offset + VirtioPciRegister::OFFSET_QUEUE_NOTIFY) as u32,
                length: (size_of::<u32>() * num_queues) as u32,
                ..Default::default()
            },
            multiplier: 0, // TODO use 4 for KVM_IOEVENTFD
        };
        let cap_device_config = VirtioPciCap {
            header: PciCapHdr {
                id: PciCapId::Vendor as u8,
                ..Default::default()
            },
            cap_len: size_of::<VirtioPciCap>() as u8,
            cfg_type: VirtioPciCfg::Device as u8,
            bar: 0,
            id: 0,
            offset: device_config_offset as u32,
            length: device_config.size() as u32,
            ..Default::default()
        };
        let msix_entries: Arc<Vec<RwLock<MsixTableEntry>>> = Arc::new(
            (0..table_entries)
                .map(|_| RwLock::new(MsixTableEntry::default()))
                .collect(),
        );
        let mut bar0 = MemRegion {
            size: 16 << 10,
            ranges: vec![],
            entries: vec![MemRegionEntry {
                size: 16 << 10,
                type_: mem::MemRegionType::Hidden,
            }],
            callbacks: Mutex::new(vec![]),
        };

        let mut caps: Vec<Box<(dyn PciCap)>> = vec![
            Box::new(MsixCapMmio {
                cap: RwLock::new(cap_msix),
            }),
            Box::new(cap_common),
            Box::new(cap_isr),
            Box::new(cap_notify),
        ];
        if device_config.size() > 0 {
            caps.push(Box::new(cap_device_config));
        }
        if let Some(region) = &dev.shared_mem_regions {
            let mut offset = 0;
            for (index, entry) in region.entries.iter().enumerate() {
                let share_mem_cap = VirtioPciCap64 {
                    cap: VirtioPciCap {
                        header: PciCapHdr {
                            id: PciCapId::Vendor as u8,
                            ..Default::default()
                        },
                        cap_len: size_of::<VirtioPciCap64>() as u8,
                        cfg_type: VirtioPciCfg::SharedMemory as u8,
                        bar: 2,
                        id: index as u8,
                        offset: offset as u32,
                        length: entry.size as u32,
                        ..Default::default()
                    },
                    length_hi: (entry.size >> 32) as u32,
                    offset_hi: (offset >> 32) as u32,
                };
                caps.push(Box::new(share_mem_cap));
                offset += entry.size;
            }
        }

        let cap_list = PciCapList::try_from(caps)?;

        let msix_vector = VirtioPciMsixVector {
            config: AtomicU16::new(VIRTIO_MSI_NO_VECTOR),
            queues: (0..num_queues)
                .map(|_| AtomicU16::new(VIRTIO_MSI_NO_VECTOR))
                .collect(),
        };

        let registers = Arc::new(VirtioPciRegisterMmio {
            name: dev.name.clone(),
            reg: dev.reg.clone(),
            event_tx: dev.event_tx.clone(),
            waker: dev.waker.clone(),
            queues: dev.queue_regs.clone(),
            irq_sender: Arc::new(PciIrqSender {
                msix_vector,
                msix_entries: msix_entries.clone(),
                msi_sender,
            }),
        });
        bar0.ranges.push(MemRange::Emulated(Arc::new(MsixTableMmio {
            entries: msix_entries,
        })));
        bar0.ranges
            .push(MemRange::Span((12 << 10) - msix_table_size));
        bar0.ranges.push(MemRange::Emulated(registers.clone()));
        if device_config.size() > 0 {
            bar0.ranges.push(MemRange::Emulated(device_config))
        }
        let mut bars = PciBar::empty_6();
        let mut bar_masks = [0; 6];
        let bar0_mask = !((bar0.size as u64).next_power_of_two() - 1);
        bar_masks[0] = bar0_mask as u32;
        bar_masks[1] = (bar0_mask >> 32) as u32;
        bars[0] = PciBar::Mem64(Arc::new(bar0));
        header.bars[0] = BAR_MEM64;

        if let Some(region) = &dev.shared_mem_regions {
            let bar2_mask = !((region.size as u64).next_power_of_two() - 1);
            bar_masks[2] = bar2_mask as u32;
            bar_masks[3] = (bar2_mask >> 32) as u32;
            bars[2] = PciBar::Mem64(region.clone());
            let mut not_emulated = |r| !matches!(r, &MemRange::Emulated(_));
            let prefetchable = region.ranges.iter().all(&mut not_emulated);
            header.bars[2] = BAR_MEM64 | if prefetchable { BAR_PREFETCHABLE } else { 0 };
        }

        let config = Arc::new(EmulatedConfig::new_device(
            header, bar_masks, bars, cap_list,
        ));

        Ok(VirtioPciDevice {
            dev,
            config,
            registers,
        })
    }
}

impl<D, M> Pci for VirtioPciDevice<D, M>
where
    M: MsiSender,
    D: Virtio,
{
    fn config(&self) -> Arc<dyn PciConfig> {
        self.config.clone()
    }
    fn reset(&self) -> pci::Result<()> {
        self.registers.wake_up_dev(WakeEvent::Reset);
        self.registers.reset();
        self.dev.reg.status.store(0, Ordering::Release);
        Ok(())
    }
}
