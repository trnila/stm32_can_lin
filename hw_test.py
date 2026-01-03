#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pytest==9.0.2",
#     "python-can==4.6.1",
# ]
# ///
import subprocess
import pytest
import can
from contextlib import contextmanager
from dataclasses import dataclass
from enum import Enum
import os
import itertools


class ChannelRole(Enum):
    RX = "rx"
    TX = "tx"


@dataclass
class DutChannelRole:
    channel: int
    role: ChannelRole


@dataclass
class CanBuses:
    rx: can.Bus
    tx: can.Bus


@dataclass
class CanMsgId:
    arbitration_id: int
    is_extended_id: bool


@dataclass
class CanFullDuplexTest:
    """Arguments for canfdtest (CAN Full-Duplex Test) program"""

    description: str
    args: list[str]


CHANNELS = [0, 1, 2]
ROLES = [ChannelRole.TX, ChannelRole.RX]
ALL_CHANNEL_ROLES = [
    DutChannelRole(chan, role) for chan, role in itertools.product(CHANNELS, ROLES)
]
CAN_FULL_DUPLEX_CONFIGS = [
    CanFullDuplexTest("standard", []),
    CanFullDuplexTest("extended", ["-e"]),
    CanFullDuplexTest("standard-1byte", ["-s", "1"]),
    CanFullDuplexTest("standard-7byte", ["-s", "7"]),
]


def open_can(iface: str, bitrate: int) -> can.Bus:
    subprocess.run(["sudo", "ip", "link", "set", iface, "down"])
    subprocess.run(
        ["sudo", "ip", "link", "set", iface, "type", "can", "bitrate", str(bitrate)]
    )
    subprocess.run(["sudo", "ip", "link", "set", iface, "up"])
    return can.Bus(interface="socketcan", channel=iface)


@contextmanager
def open_buses(channel: DutChannelRole, bitrate: int = 500000):
    channels = {}

    config = os.getenv("CAN_IFACES", "")
    if config:
        for s in config.split(","):
            chan, dut_iface, test_iface = s.split(":")
            channels[int(chan)] = (dut_iface, test_iface)

    chan = channels.get(channel.channel, None)
    if not chan:
        pytest.skip(f"CAN channel {channel.channel} not configured")

    with open_can(chan[0], bitrate) as bus_a, open_can(chan[1], bitrate) as bus_b:
        if channel.role == ChannelRole.TX:
            yield CanBuses(bus_a, bus_b)
        else:
            yield CanBuses(bus_b, bus_a)


def channel_role_id(channel_role: DutChannelRole) -> str:
    role = "tx" if channel_role.role == ChannelRole.TX else "rx"
    return f"chan{channel_role.channel}-{role}"


@pytest.mark.parametrize("dlc", range(0, 9))
@pytest.mark.parametrize("channel_role", ALL_CHANNEL_ROLES, ids=channel_role_id)
def test_dlc(channel_role: DutChannelRole, dlc: int):
    with open_buses(channel_role) as buses:
        buses.tx.send(
            can.Message(arbitration_id=42, is_extended_id=False, data=range(dlc))
        )
        received = buses.rx.recv(1)
        assert received
        assert received.arbitration_id == 42
        assert not received.is_extended_id
        assert not received.is_fd
        assert list(received.data) == list(range(dlc))


@pytest.mark.parametrize(
    "msgid",
    [
        CanMsgId(0, False),
        CanMsgId(1, True),
        CanMsgId(0x42, True),
        CanMsgId(0x42, False),
        CanMsgId(0x7FF, True),
        CanMsgId(0x7FF, False),
        CanMsgId(0x800, True),
        CanMsgId(0x1FFFFFFF, True),
    ],
    ids=lambda msg: f"0x{msg.arbitration_id:x}-{'extended' if msg.is_extended_id else 'std'}",
)
@pytest.mark.parametrize("channel_role", ALL_CHANNEL_ROLES, ids=channel_role_id)
def test_msgids(channel_role: DutChannelRole, msgid: CanMsgId):
    with open_buses(channel_role) as buses:
        buses.tx.send(
            can.Message(
                arbitration_id=msgid.arbitration_id, is_extended_id=msgid.is_extended_id
            )
        )
        received = buses.rx.recv(1)
        assert received
        assert received.arbitration_id == msgid.arbitration_id
        assert received.is_extended_id == msgid.is_extended_id
        assert received.data == bytearray([])


@pytest.mark.parametrize(
    "bitrate",
    [125000, 250000, 500000, 1000000],
)
@pytest.mark.parametrize("channel_role", ALL_CHANNEL_ROLES, ids=channel_role_id)
def test_bitrates(channel_role: str, bitrate: int):
    with open_buses(channel_role, bitrate) as buses:
        buses.tx.send(can.Message(arbitration_id=0x345, data=[1, 2, 3, 4, 5, 6, 7, 8]))
        received = buses.rx.recv(1)
        assert received.arbitration_id == 0x345
        assert list(received.data) == [1, 2, 3, 4, 5, 6, 7, 8]


@pytest.mark.parametrize("channel_role", ALL_CHANNEL_ROLES, ids=channel_role_id)
@pytest.mark.parametrize(
    "config", CAN_FULL_DUPLEX_CONFIGS, ids=lambda config: config.description
)
def test_full_duplex(channel_role: str, config: CanFullDuplexTest):
    with open_buses(channel_role) as buses:
        with subprocess.Popen(
            ["canfdtest", "-vv", buses.rx.channel] + config.args
        ) as dut:
            try:
                subprocess.check_call(
                    ["canfdtest", "-g", "-l", "1000", buses.tx.channel] + config.args,
                    timeout=5,
                )
            finally:
                dut.kill()


if __name__ == "__main__":
    pytest.main()
