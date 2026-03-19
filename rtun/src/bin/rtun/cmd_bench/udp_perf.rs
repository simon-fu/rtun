use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};

use anyhow::{anyhow, bail, ensure, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const UDP_PERF_PROTOCOL_VERSION: u8 = 1;

pub const UDP_PERF_WIRE_META_LEN: usize = 2;
pub const UDP_PERF_CONTROL_META_LEN: usize = UDP_PERF_WIRE_META_LEN + 1;
pub const UDP_PERF_CONTROL_HELLO_LEN: usize = UDP_PERF_CONTROL_META_LEN + 8 + 1;
pub const UDP_PERF_CONTROL_START_LEN: usize = UDP_PERF_CONTROL_META_LEN + 8 + 1 + 2 + 8 + 8 + 8;
pub const UDP_PERF_CONTROL_STOP_LEN: usize = UDP_PERF_CONTROL_META_LEN + 8;

pub const UDP_PERF_COUNTERS_SUMMARY_LEN: usize = 8 + 8 + 8 + 8 + 8 + 8 + 8;
pub const UDP_PERF_DIRECTION_SUMMARY_LEN: usize =
    UDP_PERF_COUNTERS_SUMMARY_LEN + UDP_PERF_COUNTERS_SUMMARY_LEN;
pub const UDP_PERF_SUMMARY_LEN: usize = 8
    + 1
    + 8
    + 8
    + UDP_PERF_COUNTERS_SUMMARY_LEN
    + UDP_PERF_COUNTERS_SUMMARY_LEN
    + UDP_PERF_DIRECTION_SUMMARY_LEN
    + UDP_PERF_DIRECTION_SUMMARY_LEN;
pub const UDP_PERF_CONTROL_REPORT_LEN: usize =
    UDP_PERF_CONTROL_META_LEN + 8 + 1 + UDP_PERF_SUMMARY_LEN;

pub const UDP_PERF_DATA_META_LEN: usize = UDP_PERF_WIRE_META_LEN + 8 + 1 + 1 + 8 + 8 + 2;

const UDP_PERF_PACKET_TYPE_CONTROL: u8 = 1;
const UDP_PERF_PACKET_TYPE_DATA: u8 = 2;
const UDP_PERF_CONTROL_KIND_HELLO: u8 = 1;
const UDP_PERF_CONTROL_KIND_START: u8 = 2;
const UDP_PERF_CONTROL_KIND_STOP: u8 = 3;
const UDP_PERF_CONTROL_KIND_REPORT: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpPerfMode {
    Forward,
    Reverse,
    Bidir,
}

impl Display for UdpPerfMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forward => write!(f, "forward"),
            Self::Reverse => write!(f, "reverse"),
            Self::Bidir => write!(f, "bidir"),
        }
    }
}

impl FromStr for UdpPerfMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "forward" => Ok(Self::Forward),
            "reverse" => Ok(Self::Reverse),
            "bidir" => Ok(Self::Bidir),
            _ => Err(format!("unsupported udp perf mode [{s}]")),
        }
    }
}

impl From<UdpPerfMode> for u8 {
    fn from(value: UdpPerfMode) -> Self {
        match value {
            UdpPerfMode::Forward => 1,
            UdpPerfMode::Reverse => 2,
            UdpPerfMode::Bidir => 3,
        }
    }
}

impl TryFrom<u8> for UdpPerfMode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Forward),
            2 => Ok(Self::Reverse),
            3 => Ok(Self::Bidir),
            _ => Err(anyhow!("unsupported udp perf mode [{value}]")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpPerfDirection {
    Forward,
    Reverse,
}

impl Display for UdpPerfDirection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forward => write!(f, "forward"),
            Self::Reverse => write!(f, "reverse"),
        }
    }
}

impl From<UdpPerfDirection> for u8 {
    fn from(value: UdpPerfDirection) -> Self {
        match value {
            UdpPerfDirection::Forward => 1,
            UdpPerfDirection::Reverse => 2,
        }
    }
}

impl TryFrom<u8> for UdpPerfDirection {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Forward),
            2 => Ok(Self::Reverse),
            _ => Err(anyhow!("unsupported udp perf direction [{value}]")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPerfHello {
    pub session_id: u64,
    pub mode: UdpPerfMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPerfStart {
    pub session_id: u64,
    pub mode: UdpPerfMode,
    pub payload_len: u16,
    pub packets_per_second: u64,
    pub duration_micros: u64,
    pub report_interval_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPerfStop {
    pub session_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UdpPerfReport {
    pub report_seq: u64,
    pub is_final: bool,
    pub summary: UdpPerfSummary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UdpPerfControlPacket {
    Hello(UdpPerfHello),
    Start(UdpPerfStart),
    Stop(UdpPerfStop),
    Report(UdpPerfReport),
}

impl UdpPerfControlPacket {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_udp_perf_control_packet(self)
    }

    pub fn decode(packet: &[u8]) -> Result<Self> {
        decode_udp_perf_control_packet(packet)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UdpPerfWirePacket {
    Control(UdpPerfControlPacket),
    Data(UdpPerfDataPacket),
}

impl UdpPerfWirePacket {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_udp_perf_wire_packet(self)
    }

    pub fn decode(packet: &[u8]) -> Result<Self> {
        decode_udp_perf_wire_packet(packet)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPerfDataHeader {
    pub session_id: u64,
    pub stream_id: u8,
    pub direction: UdpPerfDirection,
    pub seq: u64,
    pub send_ts_micros: u64,
    pub payload_len: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPerfDataPacket {
    pub header: UdpPerfDataHeader,
    pub payload: Vec<u8>,
}

impl UdpPerfDataPacket {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_udp_perf_data_packet(self)
    }

    pub fn decode(packet: &[u8]) -> Result<Self> {
        decode_udp_perf_data_packet(packet)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UdpPerfCountersSummary {
    pub bytes: u64,
    pub packets: u64,
    pub loss: u64,
    pub reorder: u64,
    pub duplicate: u64,
    pub mbps: f64,
    pub pps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UdpPerfDirectionSummary {
    pub total: UdpPerfCountersSummary,
    pub interval: UdpPerfCountersSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpPerfSummary {
    pub session_id: u64,
    pub mode: UdpPerfMode,
    pub total_elapsed_micros: u64,
    pub interval_elapsed_micros: u64,
    pub total: UdpPerfCountersSummary,
    pub interval: UdpPerfCountersSummary,
    pub forward: UdpPerfDirectionSummary,
    pub reverse: UdpPerfDirectionSummary,
}

pub fn encode_udp_perf_control_packet(packet: &UdpPerfControlPacket) -> Result<Vec<u8>> {
    let mut buf = match packet {
        UdpPerfControlPacket::Hello(_) => Vec::with_capacity(UDP_PERF_CONTROL_HELLO_LEN),
        UdpPerfControlPacket::Start(_) => Vec::with_capacity(UDP_PERF_CONTROL_START_LEN),
        UdpPerfControlPacket::Stop(_) => Vec::with_capacity(UDP_PERF_CONTROL_STOP_LEN),
        UdpPerfControlPacket::Report(_) => Vec::with_capacity(UDP_PERF_CONTROL_REPORT_LEN),
    };

    write_wire_prefix(&mut buf, UDP_PERF_PACKET_TYPE_CONTROL);

    match packet {
        UdpPerfControlPacket::Hello(hello) => {
            write_u8(&mut buf, UDP_PERF_CONTROL_KIND_HELLO);
            write_u64(&mut buf, hello.session_id);
            write_u8(&mut buf, hello.mode.into());
        }
        UdpPerfControlPacket::Start(start) => {
            write_u8(&mut buf, UDP_PERF_CONTROL_KIND_START);
            write_u64(&mut buf, start.session_id);
            write_u8(&mut buf, start.mode.into());
            write_u16(&mut buf, start.payload_len);
            write_u64(&mut buf, start.packets_per_second);
            write_u64(&mut buf, start.duration_micros);
            write_u64(&mut buf, start.report_interval_micros);
        }
        UdpPerfControlPacket::Stop(stop) => {
            write_u8(&mut buf, UDP_PERF_CONTROL_KIND_STOP);
            write_u64(&mut buf, stop.session_id);
        }
        UdpPerfControlPacket::Report(report) => {
            write_u8(&mut buf, UDP_PERF_CONTROL_KIND_REPORT);
            write_u64(&mut buf, report.report_seq);
            write_bool(&mut buf, report.is_final);
            encode_udp_perf_summary(&mut buf, &report.summary);
        }
    }

    Ok(buf)
}

pub fn decode_udp_perf_control_packet(packet: &[u8]) -> Result<UdpPerfControlPacket> {
    ensure!(
        packet.len() >= UDP_PERF_CONTROL_META_LEN,
        "udp perf control packet too short [{}] < [{}]",
        packet.len(),
        UDP_PERF_CONTROL_META_LEN
    );

    let mut packet = packet;
    let packet_type = read_u8(&mut packet)?;
    ensure!(
        packet_type == UDP_PERF_PACKET_TYPE_CONTROL,
        "unexpected udp perf packet type [{packet_type}] for control"
    );
    let version = read_u8(&mut packet)?;
    ensure!(
        version == UDP_PERF_PROTOCOL_VERSION,
        "unsupported udp perf version [{version}]"
    );

    let kind = read_u8(&mut packet)?;
    let decoded = match kind {
        UDP_PERF_CONTROL_KIND_HELLO => UdpPerfControlPacket::Hello(UdpPerfHello {
            session_id: read_u64(&mut packet)?,
            mode: UdpPerfMode::try_from(read_u8(&mut packet)?)?,
        }),
        UDP_PERF_CONTROL_KIND_START => UdpPerfControlPacket::Start(UdpPerfStart {
            session_id: read_u64(&mut packet)?,
            mode: UdpPerfMode::try_from(read_u8(&mut packet)?)?,
            payload_len: read_u16(&mut packet)?,
            packets_per_second: read_u64(&mut packet)?,
            duration_micros: read_u64(&mut packet)?,
            report_interval_micros: read_u64(&mut packet)?,
        }),
        UDP_PERF_CONTROL_KIND_STOP => UdpPerfControlPacket::Stop(UdpPerfStop {
            session_id: read_u64(&mut packet)?,
        }),
        UDP_PERF_CONTROL_KIND_REPORT => UdpPerfControlPacket::Report(UdpPerfReport {
            report_seq: read_u64(&mut packet)?,
            is_final: read_bool(&mut packet)?,
            summary: decode_udp_perf_summary(&mut packet)?,
        }),
        _ => bail!("unsupported udp perf control kind [{kind}]"),
    };

    ensure!(
        packet.is_empty(),
        "udp perf control packet has trailing bytes [{}]",
        packet.len()
    );

    Ok(decoded)
}

pub fn encode_udp_perf_data_packet(packet: &UdpPerfDataPacket) -> Result<Vec<u8>> {
    ensure!(
        packet.payload.len() == packet.header.payload_len as usize,
        "udp perf payload len mismatch [{}] != [{}]",
        packet.payload.len(),
        packet.header.payload_len
    );

    let mut buf = Vec::with_capacity(UDP_PERF_DATA_META_LEN + packet.payload.len());
    write_wire_prefix(&mut buf, UDP_PERF_PACKET_TYPE_DATA);
    write_u64(&mut buf, packet.header.session_id);
    write_u8(&mut buf, packet.header.stream_id);
    write_u8(&mut buf, packet.header.direction.into());
    write_u64(&mut buf, packet.header.seq);
    write_u64(&mut buf, packet.header.send_ts_micros);
    write_u16(&mut buf, packet.header.payload_len);
    buf.extend_from_slice(&packet.payload);
    Ok(buf)
}

pub fn decode_udp_perf_data_packet(packet: &[u8]) -> Result<UdpPerfDataPacket> {
    ensure!(
        packet.len() >= UDP_PERF_DATA_META_LEN,
        "udp perf data packet too short [{}] < [{}]",
        packet.len(),
        UDP_PERF_DATA_META_LEN
    );

    let mut packet = packet;
    let packet_type = read_u8(&mut packet)?;
    ensure!(
        packet_type == UDP_PERF_PACKET_TYPE_DATA,
        "unexpected udp perf packet type [{packet_type}] for data"
    );
    let version = read_u8(&mut packet)?;
    ensure!(
        version == UDP_PERF_PROTOCOL_VERSION,
        "unsupported udp perf version [{version}]"
    );

    let session_id = read_u64(&mut packet)?;
    let stream_id = read_u8(&mut packet)?;
    let direction = UdpPerfDirection::try_from(read_u8(&mut packet)?)?;
    let seq = read_u64(&mut packet)?;
    let send_ts_micros = read_u64(&mut packet)?;
    let payload_len = read_u16(&mut packet)? as usize;
    ensure!(
        packet.len() == payload_len,
        "udp perf data payload len mismatch [{}] != [{}]",
        packet.len(),
        payload_len
    );

    Ok(UdpPerfDataPacket {
        header: UdpPerfDataHeader {
            session_id,
            stream_id,
            direction,
            seq,
            send_ts_micros,
            payload_len: payload_len as u16,
        },
        payload: packet.to_vec(),
    })
}

pub fn encode_udp_perf_wire_packet(packet: &UdpPerfWirePacket) -> Result<Vec<u8>> {
    match packet {
        UdpPerfWirePacket::Control(packet) => packet.encode(),
        UdpPerfWirePacket::Data(packet) => packet.encode(),
    }
}

pub fn decode_udp_perf_wire_packet(packet: &[u8]) -> Result<UdpPerfWirePacket> {
    ensure!(
        packet.len() >= 1,
        "udp perf wire packet too short [{}] < [1]",
        packet.len()
    );

    match packet[0] {
        UDP_PERF_PACKET_TYPE_CONTROL => Ok(UdpPerfWirePacket::Control(
            UdpPerfControlPacket::decode(packet)?,
        )),
        UDP_PERF_PACKET_TYPE_DATA => {
            Ok(UdpPerfWirePacket::Data(UdpPerfDataPacket::decode(packet)?))
        }
        packet_type => bail!("unsupported udp perf packet type [{packet_type}]"),
    }
}

#[derive(Debug, Default)]
pub struct UdpPerfStats {
    forward: UdpPerfDirectionState,
    reverse: UdpPerfDirectionState,
}

impl UdpPerfStats {
    pub fn record_data_packet(&mut self, packet: &UdpPerfDataPacket) {
        self.direction_mut(packet.header.direction)
            .record(packet.header.seq, packet.header.payload_len as u64);
    }

    pub fn build_summary(
        &self,
        session_id: u64,
        mode: UdpPerfMode,
        total_elapsed_micros: u64,
        interval_elapsed_micros: u64,
    ) -> UdpPerfSummary {
        let forward_total = self.forward.total_counters();
        let reverse_total = self.reverse.total_counters();
        let forward_interval = self.forward.interval_counters();
        let reverse_interval = self.reverse.interval_counters();

        UdpPerfSummary {
            session_id,
            mode,
            total_elapsed_micros,
            interval_elapsed_micros,
            total: summarize_counters(forward_total + reverse_total, total_elapsed_micros),
            interval: summarize_counters(
                forward_interval + reverse_interval,
                interval_elapsed_micros,
            ),
            forward: UdpPerfDirectionSummary {
                total: summarize_counters(forward_total, total_elapsed_micros),
                interval: summarize_counters(forward_interval, interval_elapsed_micros),
            },
            reverse: UdpPerfDirectionSummary {
                total: summarize_counters(reverse_total, total_elapsed_micros),
                interval: summarize_counters(reverse_interval, interval_elapsed_micros),
            },
        }
    }

    pub fn reset_interval(&mut self) {
        self.forward.reset_interval();
        self.reverse.reset_interval();
    }

    fn direction_mut(&mut self, direction: UdpPerfDirection) -> &mut UdpPerfDirectionState {
        match direction {
            UdpPerfDirection::Forward => &mut self.forward,
            UdpPerfDirection::Reverse => &mut self.reverse,
        }
    }
}

#[derive(Debug, Default)]
struct UdpPerfDirectionState {
    total: UdpPerfTrafficCounters,
    interval: UdpPerfTrafficCounters,
    expected_seq: Option<u64>,
    last_seq: Option<u64>,
    total_gap_ranges: Vec<UdpPerfGapRange>,
    interval_gap_ranges: Vec<UdpPerfGapRange>,
    total_missing_packets: u64,
    interval_missing_packets: u64,
}

impl UdpPerfDirectionState {
    fn record(&mut self, seq: u64, bytes: u64) {
        self.total.record_packet(bytes);
        self.interval.record_packet(bytes);

        match self.expected_seq {
            None => {
                self.expected_seq = Some(seq.saturating_add(1));
                self.last_seq = Some(seq);
            }
            Some(expected_seq) if seq == expected_seq => {
                self.expected_seq = Some(seq.saturating_add(1));
                self.last_seq = Some(seq);
            }
            Some(expected_seq) if seq > expected_seq => {
                let gap = seq - expected_seq;
                append_gap_range(&mut self.total_gap_ranges, expected_seq, seq);
                append_gap_range(&mut self.interval_gap_ranges, expected_seq, seq);
                self.total_missing_packets += gap;
                self.interval_missing_packets += gap;
                self.expected_seq = Some(seq.saturating_add(1));
                self.last_seq = Some(seq);
            }
            Some(_) => {
                if self.last_seq == Some(seq) {
                    self.total.duplicate += 1;
                    self.interval.duplicate += 1;
                } else if remove_seq_from_gap_ranges(&mut self.total_gap_ranges, seq) {
                    self.total_missing_packets -= 1;
                    if remove_seq_from_gap_ranges(&mut self.interval_gap_ranges, seq) {
                        self.interval_missing_packets -= 1;
                    }
                    self.total.reorder += 1;
                    self.interval.reorder += 1;
                } else {
                    self.total.duplicate += 1;
                    self.interval.duplicate += 1;
                }
                self.last_seq = Some(seq);
            }
        }
    }

    fn total_counters(&self) -> UdpPerfRawCounters {
        UdpPerfRawCounters {
            bytes: self.total.bytes,
            packets: self.total.packets,
            loss: self.total_missing_packets,
            reorder: self.total.reorder,
            duplicate: self.total.duplicate,
        }
    }

    fn interval_counters(&self) -> UdpPerfRawCounters {
        UdpPerfRawCounters {
            bytes: self.interval.bytes,
            packets: self.interval.packets,
            loss: self.interval_missing_packets,
            reorder: self.interval.reorder,
            duplicate: self.interval.duplicate,
        }
    }

    fn reset_interval(&mut self) {
        self.interval = UdpPerfTrafficCounters::default();
        self.interval_gap_ranges.clear();
        self.interval_missing_packets = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UdpPerfGapRange {
    start: u64,
    end_exclusive: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UdpPerfTrafficCounters {
    bytes: u64,
    packets: u64,
    reorder: u64,
    duplicate: u64,
}

impl UdpPerfTrafficCounters {
    fn record_packet(&mut self, bytes: u64) {
        self.bytes += bytes;
        self.packets += 1;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UdpPerfRawCounters {
    bytes: u64,
    packets: u64,
    loss: u64,
    reorder: u64,
    duplicate: u64,
}

impl std::ops::Add for UdpPerfRawCounters {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            bytes: self.bytes + rhs.bytes,
            packets: self.packets + rhs.packets,
            loss: self.loss + rhs.loss,
            reorder: self.reorder + rhs.reorder,
            duplicate: self.duplicate + rhs.duplicate,
        }
    }
}

fn summarize_counters(counters: UdpPerfRawCounters, elapsed_micros: u64) -> UdpPerfCountersSummary {
    UdpPerfCountersSummary {
        bytes: counters.bytes,
        packets: counters.packets,
        loss: counters.loss,
        reorder: counters.reorder,
        duplicate: counters.duplicate,
        mbps: bytes_to_mbps(counters.bytes, elapsed_micros),
        pps: packets_to_pps(counters.packets, elapsed_micros),
    }
}

fn bytes_to_mbps(bytes: u64, elapsed_micros: u64) -> f64 {
    if elapsed_micros == 0 {
        0.0
    } else {
        (bytes as f64 * 8.0) / elapsed_micros as f64
    }
}

fn packets_to_pps(packets: u64, elapsed_micros: u64) -> f64 {
    if elapsed_micros == 0 {
        0.0
    } else {
        (packets as f64 * 1_000_000.0) / elapsed_micros as f64
    }
}

fn encode_udp_perf_summary(buf: &mut Vec<u8>, summary: &UdpPerfSummary) {
    write_u64(buf, summary.session_id);
    write_u8(buf, summary.mode.into());
    write_u64(buf, summary.total_elapsed_micros);
    write_u64(buf, summary.interval_elapsed_micros);
    encode_counters_summary(buf, &summary.total);
    encode_counters_summary(buf, &summary.interval);
    encode_direction_summary(buf, &summary.forward);
    encode_direction_summary(buf, &summary.reverse);
}

fn write_wire_prefix(buf: &mut Vec<u8>, packet_type: u8) {
    write_u8(buf, packet_type);
    write_u8(buf, UDP_PERF_PROTOCOL_VERSION);
}

fn append_gap_range(ranges: &mut Vec<UdpPerfGapRange>, start: u64, end_exclusive: u64) {
    if start < end_exclusive {
        ranges.push(UdpPerfGapRange {
            start,
            end_exclusive,
        });
    }
}

fn remove_seq_from_gap_ranges(ranges: &mut Vec<UdpPerfGapRange>, seq: u64) -> bool {
    for idx in 0..ranges.len() {
        let range = ranges[idx];
        if seq < range.start {
            return false;
        }

        if seq >= range.end_exclusive {
            continue;
        }

        if range.start == seq && range.end_exclusive == seq + 1 {
            ranges.remove(idx);
        } else if range.start == seq {
            ranges[idx].start += 1;
        } else if range.end_exclusive == seq + 1 {
            ranges[idx].end_exclusive -= 1;
        } else {
            let tail = UdpPerfGapRange {
                start: seq + 1,
                end_exclusive: range.end_exclusive,
            };
            ranges[idx].end_exclusive = seq;
            ranges.insert(idx + 1, tail);
        }
        return true;
    }

    false
}

fn decode_udp_perf_summary(buf: &mut &[u8]) -> Result<UdpPerfSummary> {
    Ok(UdpPerfSummary {
        session_id: read_u64(buf)?,
        mode: UdpPerfMode::try_from(read_u8(buf)?)?,
        total_elapsed_micros: read_u64(buf)?,
        interval_elapsed_micros: read_u64(buf)?,
        total: decode_counters_summary(buf)?,
        interval: decode_counters_summary(buf)?,
        forward: decode_direction_summary(buf)?,
        reverse: decode_direction_summary(buf)?,
    })
}

fn encode_direction_summary(buf: &mut Vec<u8>, summary: &UdpPerfDirectionSummary) {
    encode_counters_summary(buf, &summary.total);
    encode_counters_summary(buf, &summary.interval);
}

fn decode_direction_summary(buf: &mut &[u8]) -> Result<UdpPerfDirectionSummary> {
    Ok(UdpPerfDirectionSummary {
        total: decode_counters_summary(buf)?,
        interval: decode_counters_summary(buf)?,
    })
}

fn encode_counters_summary(buf: &mut Vec<u8>, summary: &UdpPerfCountersSummary) {
    write_u64(buf, summary.bytes);
    write_u64(buf, summary.packets);
    write_u64(buf, summary.loss);
    write_u64(buf, summary.reorder);
    write_u64(buf, summary.duplicate);
    write_f64(buf, summary.mbps);
    write_f64(buf, summary.pps);
}

fn decode_counters_summary(buf: &mut &[u8]) -> Result<UdpPerfCountersSummary> {
    Ok(UdpPerfCountersSummary {
        bytes: read_u64(buf)?,
        packets: read_u64(buf)?,
        loss: read_u64(buf)?,
        reorder: read_u64(buf)?,
        duplicate: read_u64(buf)?,
        mbps: read_f64(buf)?,
        pps: read_f64(buf)?,
    })
}

fn write_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

fn write_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_be_bytes());
}

fn write_bool(buf: &mut Vec<u8>, value: bool) {
    write_u8(buf, if value { 1 } else { 0 });
}

fn write_f64(buf: &mut Vec<u8>, value: f64) {
    buf.extend_from_slice(&value.to_bits().to_be_bytes());
}

fn read_u8(buf: &mut &[u8]) -> Result<u8> {
    let data = take_exact(buf, 1)?;
    Ok(data[0])
}

fn read_u16(buf: &mut &[u8]) -> Result<u16> {
    let data = take_exact(buf, 2)?;
    Ok(u16::from_be_bytes([data[0], data[1]]))
}

fn read_u64(buf: &mut &[u8]) -> Result<u64> {
    let data = take_exact(buf, 8)?;
    Ok(u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}

fn read_bool(buf: &mut &[u8]) -> Result<bool> {
    match read_u8(buf)? {
        0 => Ok(false),
        1 => Ok(true),
        value => bail!("invalid udp perf bool value [{value}]"),
    }
}

fn read_f64(buf: &mut &[u8]) -> Result<f64> {
    Ok(f64::from_bits(read_u64(buf)?))
}

fn take_exact<'a>(buf: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    ensure!(
        buf.len() >= len,
        "udp perf packet too short [{}] < [{}]",
        buf.len(),
        len
    );
    let (head, tail) = buf.split_at(len);
    *buf = tail;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_perf_mode_from_str_supports_issue13_modes() {
        assert_eq!(
            "forward".parse::<UdpPerfMode>().unwrap(),
            UdpPerfMode::Forward
        );
        assert_eq!(
            "reverse".parse::<UdpPerfMode>().unwrap(),
            UdpPerfMode::Reverse
        );
        assert_eq!("bidir".parse::<UdpPerfMode>().unwrap(), UdpPerfMode::Bidir);
    }

    #[test]
    fn udp_perf_control_packet_encode_decode_roundtrip() {
        let packets = vec![
            UdpPerfControlPacket::Hello(UdpPerfHello {
                session_id: 7,
                mode: UdpPerfMode::Forward,
            }),
            UdpPerfControlPacket::Start(UdpPerfStart {
                session_id: 7,
                mode: UdpPerfMode::Bidir,
                payload_len: 1200,
                packets_per_second: 2_500,
                duration_micros: 10_000_000,
                report_interval_micros: 1_000_000,
            }),
            UdpPerfControlPacket::Stop(UdpPerfStop { session_id: 7 }),
            UdpPerfControlPacket::Report(UdpPerfReport {
                report_seq: 3,
                is_final: true,
                summary: sample_summary(),
            }),
        ];

        for packet in packets {
            let encoded = packet.encode().unwrap();
            let decoded = UdpPerfControlPacket::decode(&encoded).unwrap();
            assert_eq!(decoded, packet);
        }
    }

    #[test]
    fn udp_perf_start_roundtrip_includes_reverse_stream_config() {
        let packet = UdpPerfControlPacket::Start(UdpPerfStart {
            session_id: 21,
            mode: UdpPerfMode::Reverse,
            payload_len: 1400,
            packets_per_second: 8_000,
            duration_micros: 30_000_000,
            report_interval_micros: 500_000,
        });

        let encoded = packet.encode().unwrap();
        let decoded = UdpPerfControlPacket::decode(&encoded).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn udp_perf_report_roundtrip_includes_phase_and_sequence() {
        let packet = UdpPerfControlPacket::Report(UdpPerfReport {
            report_seq: 9,
            is_final: false,
            summary: sample_summary(),
        });

        let encoded = packet.encode().unwrap();
        let decoded = UdpPerfControlPacket::decode(&encoded).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn udp_perf_wire_packet_type_distinguishes_control_and_data() {
        let control = UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(UdpPerfHello {
            session_id: 7,
            mode: UdpPerfMode::Forward,
        }));
        let data = UdpPerfWirePacket::Data(data_packet(UdpPerfDirection::Forward, 1, 8));

        let control_encoded = control.encode().unwrap();
        let data_encoded = data.encode().unwrap();

        assert_ne!(control_encoded[0], data_encoded[0]);
        assert!(matches!(
            UdpPerfWirePacket::decode(&control_encoded).unwrap(),
            UdpPerfWirePacket::Control(UdpPerfControlPacket::Hello(_))
        ));
        assert!(matches!(
            UdpPerfWirePacket::decode(&data_encoded).unwrap(),
            UdpPerfWirePacket::Data(_)
        ));
        assert!(UdpPerfControlPacket::decode(&data_encoded).is_err());
        assert!(UdpPerfDataPacket::decode(&control_encoded).is_err());
    }

    #[test]
    fn udp_perf_data_packet_encode_decode_roundtrip() {
        let packet = UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id: 11,
                stream_id: 2,
                direction: UdpPerfDirection::Reverse,
                seq: 42,
                send_ts_micros: 1_234_567,
                payload_len: 5,
            },
            payload: b"hello".to_vec(),
        };

        let encoded = packet.encode().unwrap();
        let decoded = UdpPerfDataPacket::decode(&encoded).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn udp_perf_stats_track_loss_reorder_and_duplicate() {
        let mut stats = UdpPerfStats::default();

        for seq in [1_u64, 3, 2, 3, 4] {
            stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, seq, 128));
        }

        let summary = stats.build_summary(9, UdpPerfMode::Forward, 4_000_000, 1_000_000);

        assert_eq!(summary.total.loss, 0);
        assert_eq!(summary.total.reorder, 1);
        assert_eq!(summary.total.duplicate, 1);
        assert_eq!(summary.forward.total.loss, 0);
        assert_eq!(summary.forward.total.reorder, 1);
        assert_eq!(summary.forward.total.duplicate, 1);
        assert_eq!(summary.reverse.total.loss, 0);
    }

    #[test]
    fn udp_perf_stats_track_remaining_loss_when_gap_is_not_filled() {
        let mut stats = UdpPerfStats::default();

        for seq in [1_u64, 3, 4] {
            stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, seq, 128));
        }

        let summary = stats.build_summary(11, UdpPerfMode::Forward, 3_000_000, 1_000_000);

        assert_eq!(summary.total.loss, 1);
        assert_eq!(summary.total.reorder, 0);
        assert_eq!(summary.total.duplicate, 0);
        assert_eq!(summary.forward.total.loss, 1);
        assert_eq!(summary.forward.interval.loss, 1);
    }

    #[test]
    fn udp_perf_reset_interval_clears_interval_state_and_keeps_total_state() {
        let mut stats = UdpPerfStats::default();

        for seq in [1_u64, 3] {
            stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, seq, 128));
        }

        stats.reset_interval();

        let after_reset = stats.build_summary(13, UdpPerfMode::Forward, 3_000_000, 1_000_000);
        assert_eq!(after_reset.total.loss, 1);
        assert_eq!(after_reset.interval.bytes, 0);
        assert_eq!(after_reset.interval.packets, 0);
        assert_eq!(after_reset.interval.loss, 0);
        assert_eq!(after_reset.interval.reorder, 0);

        stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, 2, 128));

        let after_fill = stats.build_summary(13, UdpPerfMode::Forward, 4_000_000, 1_000_000);
        assert_eq!(after_fill.total.loss, 0);
        assert_eq!(after_fill.total.reorder, 1);
        assert_eq!(after_fill.interval.loss, 0);
        assert_eq!(after_fill.interval.reorder, 1);
        assert_eq!(after_fill.interval.packets, 1);
    }

    #[test]
    fn udp_perf_interval_loss_is_rolled_back_when_gap_is_filled_same_interval() {
        let mut stats = UdpPerfStats::default();

        for seq in [1_u64, 3, 2] {
            stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, seq, 128));
        }

        let summary = stats.build_summary(12, UdpPerfMode::Forward, 3_000_000, 1_000_000);

        assert_eq!(summary.total.loss, 0);
        assert_eq!(summary.total.reorder, 1);
        assert_eq!(summary.forward.interval.loss, 0);
        assert_eq!(summary.forward.interval.reorder, 1);
        assert_eq!(summary.interval.loss, 0);
        assert_eq!(summary.interval.reorder, 1);
    }

    #[test]
    fn udp_perf_large_gap_uses_single_gap_range_representation() {
        let mut stats = UdpPerfStats::default();

        stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, 1, 128));
        stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, 1_000_000, 128));

        assert_eq!(stats.forward.total_gap_ranges.len(), 1);
        assert_eq!(stats.forward.interval_gap_ranges.len(), 1);
        assert_eq!(stats.forward.total_missing_packets, 999_998);
        assert_eq!(stats.forward.interval_missing_packets, 999_998);

        let summary = stats.build_summary(14, UdpPerfMode::Forward, 2_000_000, 1_000_000);
        assert_eq!(summary.total.loss, 999_998);
        assert_eq!(summary.interval.loss, 999_998);
    }

    #[test]
    fn udp_perf_interval_stats_accumulate_bytes_packets_mbps_and_pps() {
        let mut stats = UdpPerfStats::default();

        stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, 1, 62_500));
        stats.record_data_packet(&data_packet(UdpPerfDirection::Forward, 2, 62_500));

        let summary = stats.build_summary(10, UdpPerfMode::Forward, 1_000_000, 1_000_000);

        assert_eq!(summary.interval.bytes, 125_000);
        assert_eq!(summary.interval.packets, 2);
        assert!((summary.interval.mbps - 1.0).abs() < f64::EPSILON);
        assert!((summary.interval.pps - 2.0).abs() < f64::EPSILON);
        assert_eq!(summary.forward.interval.bytes, 125_000);
        assert_eq!(summary.forward.interval.packets, 2);
    }

    fn data_packet(direction: UdpPerfDirection, seq: u64, payload_len: usize) -> UdpPerfDataPacket {
        UdpPerfDataPacket {
            header: UdpPerfDataHeader {
                session_id: 99,
                stream_id: 1,
                direction,
                seq,
                send_ts_micros: seq * 1_000,
                payload_len: payload_len as u16,
            },
            payload: vec![0x5a; payload_len],
        }
    }

    fn sample_summary() -> UdpPerfSummary {
        UdpPerfSummary {
            session_id: 7,
            mode: UdpPerfMode::Bidir,
            total_elapsed_micros: 5_000_000,
            interval_elapsed_micros: 1_000_000,
            total: UdpPerfCountersSummary {
                bytes: 10_000,
                packets: 100,
                loss: 2,
                reorder: 1,
                duplicate: 3,
                mbps: 0.016,
                pps: 20.0,
            },
            interval: UdpPerfCountersSummary {
                bytes: 2_000,
                packets: 20,
                loss: 1,
                reorder: 1,
                duplicate: 0,
                mbps: 0.016,
                pps: 20.0,
            },
            forward: UdpPerfDirectionSummary {
                total: UdpPerfCountersSummary {
                    bytes: 7_000,
                    packets: 70,
                    loss: 1,
                    reorder: 1,
                    duplicate: 2,
                    mbps: 0.0112,
                    pps: 14.0,
                },
                interval: UdpPerfCountersSummary {
                    bytes: 1_400,
                    packets: 14,
                    loss: 1,
                    reorder: 1,
                    duplicate: 0,
                    mbps: 0.0112,
                    pps: 14.0,
                },
            },
            reverse: UdpPerfDirectionSummary {
                total: UdpPerfCountersSummary {
                    bytes: 3_000,
                    packets: 30,
                    loss: 1,
                    reorder: 0,
                    duplicate: 1,
                    mbps: 0.0048,
                    pps: 6.0,
                },
                interval: UdpPerfCountersSummary {
                    bytes: 600,
                    packets: 6,
                    loss: 0,
                    reorder: 0,
                    duplicate: 0,
                    mbps: 0.0048,
                    pps: 6.0,
                },
            },
        }
    }
}
