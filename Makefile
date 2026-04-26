TARGET = x86_64-pc-windows-gnu
RELEASE_DIR = teleport/target/$(TARGET)/release

# Default: cross-compile for Windows
all: build

# Install the required target (one-time setup)
setup:
	rustup target add $(TARGET)
	brew list mingw-w64 || brew install mingw-w64

lint:
	cd teleport && cargo fmt --all
	cd teleport && cargo clippy --target=$(TARGET) --all-targets --all-features --fix --allow-dirty

# Cross compile using cargo
build:
	cd teleport && cargo build --target=$(TARGET) --release

# Run tests of cross-platform bits
test:
	cd teleport && cargo test --target=$(TARGET) --release

# Remove compiled artifacts
clean:
	cd teleport && cargo clean

# Show the output binary paths
print:
	@echo "source: $(RELEASE_DIR)/source.exe"
	@echo "target: $(RELEASE_DIR)/target.exe"