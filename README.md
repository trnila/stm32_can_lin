# STM32 Triple CAN/LIN USB bridge
STM32G4xx firmware written in Rust using the Embassy framework that bridges CAN buses to Linux via USB using the `gs_usb` driver and socketCAN interface.

- 3x CAN channels

## Build & flash & run
Install Rust via [rustup.rs](https://rustup.rs/).

Set `nSWBOOT0` to 0 using STM32CubeProgrammer for the first time, then connect the debugger and flash the device:
```sh
$ cargo run --release
```

Three socketCAN interfaces will appear, for example as `can0`, `can1`, and `can2`.
You can configure them using `ip` command:
```sh
$ sudo ip link set can0 type can bitrate 500000
$ sudo ip link set can0 up
```

After that, you can start sending and receiving CAN frames:
```sh
$ candump can0
$ cansend can0 123#abcd
```

## Test
Install following dependencies:
- uv

Next, connect the DUT's `can0` interface to the tester's `can1_0` interface, and so on. Then run:
```sh
$ CAN_IFACES=0:can0:can1_0,1:can1:can1_1,2:can2:can0_0 ./hw_test.py
```
