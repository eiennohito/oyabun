# Dev-only: rebuild the committed BPF object (atop.bpf.o).
#
# Normal `cargo build` does NOT run this — it embeds the committed .o via include_bytes!.
# Rerun (`make` here, or `just bpf` from the repo root) only after editing atop.bpf.c or
# atop_types.h, then commit the regenerated .o alongside the source. This keeps clang +
# libbpf headers out of the standard build, exactly like a committed protobuf-generated file.
#
# vmlinux.h is a *build-only* input (NOT committed, NOT needed at runtime): the committed .o
# carries its own BTF and the loader relocates against the live kernel's BTF via CO-RE. It is
# generated on demand from the builder's own kernel below — the only correct source for it.

CLANG     ?= clang
LLVM_STRIP ?= llvm-strip
ARCH      := $(shell uname -m | sed 's/x86_64/x86/;s/aarch64/arm64/;s/ppc64le/powerpc/')

BPF_CFLAGS := -target bpf -D__TARGET_ARCH_$(ARCH) -mcpu=v3 -O2 -g -Wall -Werror

atop.bpf.o: atop.bpf.c atop_types.h vmlinux.h
	$(CLANG) $(BPF_CFLAGS) -c $< -o $@
	$(LLVM_STRIP) -g $@   # drop DWARF, keep the .BTF section CO-RE needs

# Generated from the running kernel's BTF; auto-built as a prerequisite if absent.
vmlinux.h:
	bpftool btf dump file /sys/kernel/btf/vmlinux format c > $@

.PHONY: vmlinux clean
vmlinux: # force-regenerate (e.g. after a kernel upgrade)
	rm -f vmlinux.h && $(MAKE) vmlinux.h

clean:
	rm -f atop.bpf.o
