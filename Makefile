PREFIX ?= $(HOME)/.local

.PHONY: build check install uninstall
build:
	cargo build --release --locked
check:
	cargo fmt --all --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-features
install: build
	install -Dm755 target/release/resolve-ingest $(DESTDIR)$(PREFIX)/bin/resolve-ingest
	@if [ -n "$(FFMPEG_DIR)" ]; then install -d $(DESTDIR)$(PREFIX)/libexec/resolve-ingest && install -m755 $(FFMPEG_DIR)/ffmpeg $(DESTDIR)$(PREFIX)/libexec/resolve-ingest/ffmpeg && install -m755 $(FFMPEG_DIR)/ffprobe $(DESTDIR)$(PREFIX)/libexec/resolve-ingest/ffprobe; fi
	install -Dm644 packaging/io.github.resolve_ingest.ResolveIngest.desktop $(DESTDIR)$(PREFIX)/share/applications/io.github.resolve_ingest.ResolveIngest.desktop
	install -Dm644 packaging/io.github.resolve_ingest.ResolveIngest.svg $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.resolve_ingest.ResolveIngest.svg
uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/resolve-ingest $(DESTDIR)$(PREFIX)/libexec/resolve-ingest/ffmpeg $(DESTDIR)$(PREFIX)/libexec/resolve-ingest/ffprobe $(DESTDIR)$(PREFIX)/share/applications/io.github.resolve_ingest.ResolveIngest.desktop $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.resolve_ingest.ResolveIngest.svg
