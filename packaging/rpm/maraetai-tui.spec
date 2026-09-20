# This targets a COPR repo (Fedora's community/AUR-equivalent), not
# official Fedora — official repos require every dependency crate split
# into its own rust-* sub-package, which is a much larger undertaking than
# this project currently needs. `cargo build` is left free to reach
# crates.io during %build, so the COPR project this gets built in must
# have "Enable networking during builds" turned on (Settings > General on
# the COPR project) — off by default, and the build will fail without it.
#
# No release has been tagged yet, so this follows Fedora's convention for
# packaging a git snapshot (see "Snapshot or pre-release package" in the
# Fedora Versioning guidelines): Version is 0^<date>git<shortcommit>, and
# bumping to a newer commit means updating commit0/gitdate below and
# running `rpmdev-bumpspec` (or bumping Release by hand) — there's no
# auto-detection at build time the way the AUR PKGBUILD's pkgver() does,
# because rpmbuild has no equivalent hook that runs before Version is
# fixed.
%global commit0 f14d13192ab2f2044665d1f0fe5124e96c871653
%global shortcommit0 %(c=%{commit0}; echo ${c:0:12})
%global gitdate 20260920

Name:           maraetai-tui
Version:        0^%{gitdate}git%{shortcommit0}
Release:        1%{?dist}
Summary:        Terminal client for a Navidrome library, with a background playback daemon

License:        MIT
URL:            https://github.com/unclefroob/maraetai-tui
Source0:        %{url}/archive/%{commit0}/%{name}-%{shortcommit0}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  alsa-lib-devel
BuildRequires:  dbus-devel
BuildRequires:  pkgconf-pkg-config
Requires:       alsa-lib
Requires:       dbus-libs

%description
A Subsonic/Navidrome terminal client (%{name}) backed by a background
playback daemon, giving MPRIS and media-key integration independent of
whether the terminal UI itself is open. Installs two binaries: maraetai
(the TUI) and maraetaid (the daemon it talks to).

%prep
%autosetup -n %{name}-%{commit0}

%build
# Rust code is compiled by rustc, not gcc, so %{optflags}/LTO settings
# from redhat-rpm-config only reach this build via build-script-compiled C
# (e.g. the `ring` crate's hand-written crypto, pulled in transitively by
# reqwest's rustls-tls backend). Fedora's default -flto is known to break
# linking such crates the same way Arch's makepkg LTO option does (GCC
# emits LTO-bytecode-only objects that rust-lld can't read) — so LTO is
# disabled for the C side via CFLAGS/CXXFLAGS rather than trying to keep
# it and fight the linker.
export CFLAGS="%{optflags}"
export CFLAGS="${CFLAGS//-flto*/}"
export CXXFLAGS="$CFLAGS"
cargo build --release --locked --workspace

%check
export CFLAGS="%{optflags}"
export CFLAGS="${CFLAGS//-flto*/}"
export CXXFLAGS="$CFLAGS"
# Fully self-contained (mock HTTP servers on 127.0.0.1, no real audio
# device or network needed) — safe in a mock/koji-style build root.
cargo test --release --locked --workspace

%install
install -Dm755 target/release/maraetai %{buildroot}%{_bindir}/maraetai
install -Dm755 target/release/maraetaid %{buildroot}%{_bindir}/maraetaid

%files
%license LICENSE
%doc README.md
%{_bindir}/maraetai
%{_bindir}/maraetaid

%changelog
* Sun Sep 20 2026 Ryan <ryanyhkwan@gmail.com> - 0^20260920gitf14d13192ab2-1
- Initial packaging, as a git snapshot (no tagged releases yet)
