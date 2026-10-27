# Recorded application replays

The input recordings and expected screens come from the Alacritty contributors' Apache-2.0 test data:

https://github.com/alacritty/alacritty/tree/94e7c8874e526b1e67b349d9ba30ddf81669119e/alacritty_terminal/tests/ref

See the root `LICENSE`. Only test data is retained here. Building and running nextty does not depend on Alacritty, and its terminal core was not copied.

Each `.bin` preserves the original PTY output. Its `.rs` expectation normalizes the corresponding `grid.json` into the visible screen: it selects the current screen rows from circular storage, reverses them into top-to-bottom order, and stores each cell's character, foreground/background color identity, visible styles and combining characters. Tabs compare as displayed spaces. Style runs merge only adjacent equal attributes; whitespace is retained. The old implementation's caches, row-storage direction and WRAPLINE metadata do not constrain the new implementation. Separate protocol tests cover wrapping and history semantics.

The static Rust data is the sole expected-screen authority; replay tests need no JSON parser.

Each recording is replayed both as one chunk and in seven-byte chunks, checking real application sequences and parsing across reads. These tests compare terminal state, not GUI, input-method or GPU behavior.
