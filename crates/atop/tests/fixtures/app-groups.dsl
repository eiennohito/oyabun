# Minimized from /tmp/atop-cgroup-capture.dsl (2026-07-10). PIDs, ancestry,
# ownership, unit shapes, and representative commands retain the captured relationships.
cycle 0 cores=32 mem=64G
  1083 uid=1000 start=100 comm=systemd cmd="/usr/lib/systemd/systemd --user"

  1684 ppid=1083 uid=1000 start=1684 threads=45 mem=745M comm=chrome cmd=/opt/google/chrome/chrome cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.google.Chrome-1684.scope\n"
  1690 ppid=1684 uid=1000 start=1690 comm=chrome cmd="/opt/google/chrome/chrome --type=renderer" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.google.Chrome-1684.scope\n"
  1718 ppid=1083 uid=1000 start=1718 comm=chrome_crashpad cmd=/opt/google/chrome/chrome_crashpad_handler cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-google\x5cx2dchrome@4891297080ec40e59b9cd18e09d270a0.service\n"
  1721 ppid=1083 uid=1000 start=1721 comm=chrome_crashpad cmd=/opt/google/chrome/chrome_crashpad_handler cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-google\x5cx2dchrome@4891297080ec40e59b9cd18e09d270a0.service\n"
  1730 ppid=1684 uid=1000 start=1730 comm=foreign cmd=/usr/bin/foreign cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foreign-1730.scope\n"

  1757 ppid=1083 uid=1000 start=1757 threads=39 comm=code cmd=/usr/share/code/code cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-code-1757.scope\n"
  1760 ppid=1757 uid=1000 start=1760 comm=code cmd="/usr/share/code/code --type=zygote" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-code@fac833b0b071487388e5d9624f8b1697.service\n"
  1820 ppid=1083 uid=1000 start=1820 comm=chrome_crashpad cmd=/usr/share/code/chrome_crashpad_handler cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-code@fac833b0b071487388e5d9624f8b1697.service\n"
  1884 ppid=1760 uid=1000 start=1884 comm=code cmd="/usr/share/code/code --type=gpu-process" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-code@fac833b0b071487388e5d9624f8b1697.service\n"

  12962 ppid=1083 uid=1000 start=12962 comm=bash cmd="bash steam.sh" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@f3ba2eea1e2f4366a685e7ada489d568.service\n"
  13407 ppid=12962 uid=1000 start=13407 comm=steam cmd=/home/user/.local/share/Steam/steam cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@f3ba2eea1e2f4366a685e7ada489d568.service\n"
  13655 ppid=13407 uid=1000 start=13655 comm=steamwebhelper cmd=./steamwebhelper cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@f3ba2eea1e2f4366a685e7ada489d568.service\n"

  23042 ppid=1083 uid=1000 start=23042 comm=bwrap cmd="bwrap -- com.slack.Slack" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.slack.Slack-4016588706.scope\n" flatpak="[Application]\nname=com.slack.Slack\n"
  23066 ppid=23042 uid=1000 start=23066 comm=slack cmd=/app/extra/slack cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.slack.Slack-4016588706.scope\n" flatpak="[Application]\nname=com.slack.Slack\n"
  23095 ppid=23042 uid=0 start=23095 comm=slack cmd="/app/extra/slack --type=zygote" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.slack.Slack-1619371975.scope\n" flatpak="[Application]\nname=com.slack.Slack\n"
  23156 ppid=23095 uid=0 start=23156 comm=slack cmd="/app/extra/slack --type=renderer" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.slack.Slack-1619371975.scope\n" flatpak="[Application]\nname=com.slack.Slack\n"

  64567 ppid=1083 uid=1000 start=64567 threads=67 comm=zed-editor cmd=/usr/lib/zed/zed-editor cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-dev.zed.Zed@75bbfca4efa54a2eab59399d9d3d7107.service\n"
  64628 ppid=64567 uid=1000 start=64628 comm=zed-editor cmd="/usr/lib/zed/zed-editor --crash-handler" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-dev.zed.Zed@75bbfca4efa54a2eab59399d9d3d7107.service\n"
  64735 ppid=64567 uid=1000 start=64735 comm=clangd cmd=/usr/bin/clangd cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-dev.zed.Zed@75bbfca4efa54a2eab59399d9d3d7107.service\n"

  2278 ppid=1083 uid=1000 start=2278 comm=kitty cmd=/usr/bin/kitty cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty@31c4ed8e30ea42c69d158330670f4abc.service\n"
  2305 ppid=2278 uid=1000 start=2305 comm=kitten cmd="/usr/bin/kitten __atexit__" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty@31c4ed8e30ea42c69d158330670f4abc.service\n"
  2308 ppid=2278 uid=1000 start=2308 comm=kitten cmd="/usr/bin/kitten __watch_conf__" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty@31c4ed8e30ea42c69d158330670f4abc.service\n"
