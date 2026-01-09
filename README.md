# Dualnand

This repository contains firmware for the [tulip dualnand](https://github.com/ambraglow/tulip).

## Compiling

Clone the repository, and build the project with:

```
cargo build -p flasher -r
```

The compiled artifacts can be found at `target/thumbv6m-none-eabi/release/flasher`.

### Flashing the compiled artifact

To flash the compiled firmware, it first needs to be converted into `uf2`:

```
picotool uf2 convert -t elf target/thumbv6m-none-eabi/release/flasher flasher.uf2
```

Connect the dualnand board to your PC with a USB cable and drag the `.uf2` file onto the drive that appears.

## Credits

* [embassy project](https://github.com/embassy-rs/embassy/)
