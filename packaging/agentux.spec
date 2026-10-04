# RPM for the AgentUX image (Fedora Kinoite 44): aux, agentuxd and the
# agentuxd systemd user unit.
#
# The binaries are built beforehand with cargo (toolchain pinned by
# rust-toolchain.toml, --locked) and this spec only lays them out, so it
# expects to be called from the workspace root as:
#
#   rpmbuild -bb packaging/agentux.spec \
#     --define "_topdir $PWD/target/rpmbuild" \
#     --define "srcroot $PWD" \
#     --define "version_ $(workspace version from Cargo.toml)"
#
# .github/workflows/release.yml does exactly that.

%global debug_package %{nil}

Name:           agentux
Version:        %{version_}
Release:        1%{?dist}
Summary:        AgentUX orchestration daemon (agentuxd) and CLI (aux)
License:        Apache-2.0
URL:            https://github.com/agentux-os/agentux-core
ExclusiveArch:  x86_64 aarch64

BuildRequires:  systemd-rpm-macros
# Run worktrees are created by shelling out to git.
Requires:       git
# Pull requests are opened with the GitHub CLI.
Recommends:     gh

%description
The orchestration engine of AgentUX: agentuxd runs pipelines declared in a
project's agentux.yaml as persisted state machines in git worktrees, and aux
drives it over a local Unix socket. Includes a systemd user unit for agentuxd.

%install
install -Dpm 0755 %{srcroot}/target/release/aux      %{buildroot}%{_bindir}/aux
install -Dpm 0755 %{srcroot}/target/release/agentuxd %{buildroot}%{_bindir}/agentuxd
install -Dpm 0644 %{srcroot}/contrib/agentuxd.service %{buildroot}%{_userunitdir}/agentuxd.service
install -Dpm 0644 %{srcroot}/LICENSE   %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dpm 0644 %{srcroot}/README.md %{buildroot}%{_docdir}/%{name}/README.md
install -Dpm 0644 %{srcroot}/docs/api.md %{buildroot}%{_docdir}/%{name}/api.md

%post
%systemd_user_post agentuxd.service

%preun
%systemd_user_preun agentuxd.service

%files
%{_bindir}/aux
%{_bindir}/agentuxd
%{_userunitdir}/agentuxd.service
%dir %{_licensedir}/%{name}
%license %{_licensedir}/%{name}/LICENSE
%dir %{_docdir}/%{name}
%doc %{_docdir}/%{name}/README.md
%doc %{_docdir}/%{name}/api.md
