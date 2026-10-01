#!/bin/sh
# Starts Redis from the Redis conf, first switching an existing RDB-only data volume to AOF safely.
#
# Redis started with `appendonly yes` and no AOF manifest starts empty: it ignores the RDB file,
# and its next save overwrites that file with the empty data set. So when the conf asks for AOF,
# the data directory holds an RDB file, and there's no AOF manifest yet, this script backs the
# RDB file up, starts Redis with AOF off (loading the RDB), turns AOF on at runtime, and waits for
# the rewrite that writes the AOF base and manifest. Later starts find the manifest and exec Redis
# directly. If the volume lacks the free space the switch needs, or the switch fails, Redis serves
# with AOF off (its RDB saves still run) and the next start tries again. Every step is logged on
# stdout as one line starting with `redis-start:`.
#
# Usage: sh redis-start.sh [path to the Redis conf [redis-server arguments...]]
#
# The arguments after the conf go to every redis-server it starts, ahead of the `--appendonly no`
# a switch adds. The settings the script acts on are read from the conf and the files it includes,
# not from those arguments.
conf="${1:-/var/lib/redis/redis-aof.conf}"
[ "$#" -gt 0 ] && shift
log() {
  echo "redis-start: $(date -u '+%Y-%m-%dT%H:%M:%SZ') $*"
}
# print the values of the directive lines read on stdin, unquoted the way Redis reads them
unquote() {
  sed -e 's/^[ \t]*[^ \t]*[ \t]*//' -e 's/[ \t]*$//' -e '/^".*"$/ { s/^"//; s/"$//; s/\\\(.\)/\1/g; }'
}
# print a conf file with each include directive replaced, in place, by the files it names (a path
# or a glob), recursively and at most 10 levels deep, as Redis reads them; $2 is how many
# includes deep the file $1 is
expand() {
  [ "$2" -le 10 ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      *[Ii][Nn][Cc][Ll][Uu][Dd][Ee]*)
        if [ "$(printf '%s\n' "$line" | awk '{ print tolower($1) }')" = include ]; then
          inc=$(printf '%s\n' "$line" | unquote)
          # expand the glob without splitting the path on spaces
          # shellcheck disable=SC2086
          (
            IFS=''
            for f in $inc; do
              [ -f "$f" ] && expand "$f" $(($2 + 1))
            done
          )
          continue
        fi
        ;;
    esac
    printf '%s\n' "$line"
  done <"$1"
}
text=$(expand "$conf" 0)
# print every value a directive has in the conf text and its includes, in the order Redis reads them
values() {
  printf '%s\n' "$text" | awk -v key="$1" 'tolower($1) == key' | unquote
}
# print the value Redis uses for a directive: the last one it reads
setting() {
  values "$1" | tail -n 1
}
appendonly=$(setting appendonly)
rdb=$(setting dbfilename)
rdb="${rdb:-dump.rdb}"
aofdir=$(setting appenddirname)
aofdir="${aofdir:-appendonlydir}"
aoffile=$(setting appendfilename)
aoffile="${aoffile:-appendonly.aof}"
port=$(setting port)
port="${port:-6379}"
# resolve the data directory the way redis-server does: each dir directive changes the working
# directory, starting from the one the server starts in, which is this script's (it runs
# redis-server without changing directory); a dir that doesn't exist leaves it empty
dir=$(values dir | {
  while IFS= read -r d; do
    cd -- "$d" 2>/dev/null || exit 0
  done
  pwd
})
# whether the AOF manifest Redis loads at startup exists: only <appendfilename>.manifest counts, since
# Redis first writes the manifest as temp-<appendfilename>.manifest and renames it, and starts empty
# when only that temporary file is there (a kill during the first manifest write leaves it)
manifest_exists() {
  [ -f "$dir/$aofdir/$aoffile.manifest" ]
}
# the plain start: AOF off, AOF already on disk, or nothing on disk to lose
if [ "$appendonly" != "yes" ] || [ -z "$dir" ] || [ ! -f "$dir/$rdb" ] || manifest_exists; then
  exec redis-server "$conf" "$@"
fi
# redis-cli takes the password from the environment so it isn't on the command line: requirepass
# from the conf or an included auth file, or else REDIS_PASSWORD, for a password passed to Redis as
# `--requirepass` among the arguments
pass=$(setting requirepass)
pass="${pass:-${REDIS_PASSWORD:-}}"
if [ -n "$pass" ]; then
  REDISCLI_AUTH="$pass"
  export REDISCLI_AUTH
fi
cli() {
  redis-cli -h 127.0.0.1 -p "$port" "$@"
}
# read a field of the last INFO reply
field() {
  printf '%s\n' "$info" | awk -F: -v k="$1" '$1 == k { print $2 }'
}
# print the INFO persistence fields that tell why a rewrite or save failed
failure_fields() {
  for k in aof_last_bgrewrite_status aof_last_write_status rdb_last_bgsave_status aof_rewrites_consecutive_failures; do
    v=$(field "$k")
    [ -n "$v" ] && printf '%s:%s ' "$k" "$v"
  done
}
# print the total size in bytes of the given regular files, or of the regular files in the given
# directories (only the size column of ls is read, so file names don't matter)
size_of() {
  # shellcheck disable=SC2012
  ls -ln "$@" 2>/dev/null | awk '$1 ~ /^-/ { s += $5 } END { printf "%.0f\n", s }'
}
# print the bytes free to this user on the file system holding the given path, or nothing if df
# can't tell
free_on() {
  df -Pk "$1" 2>/dev/null | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { printf "%.0f\n", $4 * 1024 }'
}
# print a byte count followed by a human-readable size, such as "19390 bytes (18.9 KiB)"
human() {
  awk -v n="$1" 'BEGIN {
    split("bytes KiB MiB GiB TiB", u, " ")
    v = n
    i = 1
    while (v >= 1024 && i < 5) { v /= 1024; i++ }
    if (i == 1) printf "%.0f bytes\n", n; else printf "%.0f bytes (%.1f %s)\n", n, v, u[i]
  }'
}
# forward SIGTERM and SIGINT to Redis so its shutdown save runs. Redis refuses to shut down while
# it writes the first AOF, so until the manifest exists AOF is turned off first: that stops the
# rewrite, the shutdown saves the full data set to the RDB file, and the next start switches again
# (while Redis is still loading, CONFIG SET just fails and AOF is off anyway).
pid=""
stopping=0
caught=0
aof_stale=0
# move the AOF directory aside (never deleting it) once AOF was turned off after the rewrite had
# already written the manifest: the AOF then lacks the writes made since, and the next start would
# load it, so this makes the next start switch again from the RDB file
move_stale_aof() {
  stale="$dir/$aofdir.stale-$(date -u '+%Y%m%dT%H%M%SZ')"
  if mv "$dir/$aofdir" "$stale"; then
    log "moved $dir/$aofdir to $stale: AOF was turned off after it was written, so it lacks later writes; the next start switches again from $rdb"
  else
    log "could not move $dir/$aofdir aside; move it away from $dir before the next start, or that start loads an AOF without the latest writes"
  fi
}
# shellcheck disable=SC2329
stop() {
  stopping=1
  caught=1
  [ -n "$pid" ] || return 0
  if manifest_exists; then
    log "stopping Redis"
  else
    cli config set appendonly no >/dev/null 2>&1
    # the rewrite may have written the manifest just before AOF was turned off; no rewrite
    # finishes once AOF is off, so the check is final
    manifest_exists && aof_stale=1
    log "stopping Redis during the switch to AOF: turned AOF off so its shutdown saves $rdb; the next start tries the switch again"
  fi
  kill -TERM "$pid" 2>/dev/null
}
trap stop TERM INT
log "appendonly is yes but $dir has $rdb and no AOF manifest $dir/$aofdir/$aoffile.manifest; switching it to AOF"
# reuse an identical earlier copy of the RDB file as the backup, or name a new one
backup=""
for b in "$dir/$rdb".pre-aof-*; do
  if [ -f "$b" ] && command -v cmp >/dev/null 2>&1 && cmp -s "$b" "$dir/$rdb"; then
    backup="$b"
  fi
done
copy_exists=0
if [ -n "$backup" ]; then
  copy_exists=1
else
  backup="$dir/$rdb.pre-aof-$(date -u '+%Y%m%dT%H%M%SZ')"
fi
# drop the temporary files of a rewrite cut short by a crash (Redis isn't running yet, and it
# leaves them behind)
for t in "$dir"/temp-rewriteaof-*.aof; do
  if [ -f "$t" ]; then
    rm -f "$t"
    log "removed $t, left by an interrupted AOF rewrite"
  fi
done
# check the volume has room for the switch: the backup copy (unless one exists), the AOF base (an
# RDB-format copy of the data), the temporary file of the next RDB save, which is written in full
# before it replaces the RDB file, and a 64 MiB margin for writes made meanwhile; each is about the
# RDB file's size, so this needs 3 times its size free (2 times when the copy already exists)
rdb_size=$(size_of "$dir/$rdb")
need=$(((3 - copy_exists) * rdb_size + 64 * 1024 * 1024))
free=$(free_on "$dir")
log "data dir $dir, RDB file $dir/$rdb ($(human "$rdb_size")), AOF dir $dir/$aofdir, backup $backup"
if [ -z "$free" ]; then
  log "could not read the free space on $dir (df -P); switching anyway"
elif [ "$free" -lt "$need" ]; then
  log "not enough space on $dir to switch to AOF: needs $(human "$need"), has $(human "$free") free; starting with AOF off (RDB saves only), no backup taken. Grow the volume (PVC) or free space on it, then restart the pod; the next start tries again"
  exec redis-server "$conf" "$@" --appendonly no
else
  log "$dir has $(human "$free") free, the switch needs about $(human "$need")"
fi
# keep a copy of the RDB file until the AOF holds the data
if [ "$copy_exists" = 1 ]; then
  log "$backup already holds a copy of $rdb"
else
  if ! cp -p "$dir/$rdb" "$backup.partial" || ! mv "$backup.partial" "$backup"; then
    rm -f "$backup.partial"
    log "could not back up $rdb to $backup; starting with AOF off (RDB saves only), the next start tries again"
    exec redis-server "$conf" "$@" --appendonly no
  fi
  log "backed up $rdb to $backup"
fi
# a signal during the backup ends the start here, before Redis runs
if [ "$stopping" = 1 ]; then
  log "stopped before starting Redis"
  exit 0
fi
redis-server "$conf" "$@" --appendonly no &
pid=$!
log "started Redis with AOF off (pid $pid)"
# forward a signal that came while Redis was being started
if [ "$stopping" = 1 ]; then
  stop
fi
# wait for Redis to load the RDB file and answer, giving up on the switch if it refuses the password
ready=0
while [ "$stopping" = 0 ] && kill -0 "$pid" 2>/dev/null; do
  reply=$(cli ping 2>&1)
  case "$reply" in
    PONG)
      ready=1
      break
      ;;
    *NOAUTH* | *WRONGPASS* | *"invalid password"*)
      log "redis-cli could not authenticate: $reply"
      break
      ;;
  esac
  sleep 1
done
# turn AOF on at runtime and wait for the rewrite that writes its base file and manifest
switched=0
if [ "$ready" = 1 ]; then
  log "loaded $rdb ($(cli dbsize) keys); turning AOF on"
  reply=$(cli config set appendonly yes 2>&1)
  if [ "$reply" != "OK" ]; then
    log "CONFIG SET appendonly yes failed: $reply"
  else
    started=$(date +%s)
    report=$((started + 30))
    log "AOF rewrite started; it writes the data to $dir/$aofdir"
    while [ "$stopping" = 0 ] && kill -0 "$pid" 2>/dev/null; do
      info=$(cli info persistence 2>/dev/null | tr -d '\r')
      # a failed rewrite leaves aof_rewrite_scheduled:1 since Redis retries it, so an error with no
      # rewrite running ends the switch (the status starts as ok, so it is this run's rewrite)
      if [ "$(field aof_rewrite_in_progress)" = 0 ] && [ "$(field aof_last_bgrewrite_status)" = err ]; then
        log "AOF rewrite failed after $(($(date +%s) - started))s ($(failure_fields)$(human "$(free_on "$dir")") free on $dir); the Redis log lines above give the cause"
        break
      fi
      if [ "$(field aof_rewrite_in_progress)" = 0 ] && [ "$(field aof_rewrite_scheduled)" = 0 ] \
        && [ "$(field aof_enabled)" = 1 ] && manifest_exists; then
        switched=1
        log "AOF rewrite finished in $(($(date +%s) - started))s; $dir/$aofdir holds $(human "$(size_of "$dir/$aofdir")")"
        break
      fi
      # report a long rewrite's progress every 30 seconds
      if [ "$(date +%s)" -ge "$report" ]; then
        report=$((report + 30))
        log "AOF rewrite running for $(($(date +%s) - started))s: $(field current_save_keys_processed) of $(field current_save_keys_total) keys written"
      fi
      sleep 1
    done
  fi
fi
if [ "$switched" = 1 ]; then
  log "AOF is on and $dir/$aofdir holds the data; $backup ($(human "$(size_of "$backup")")) can be deleted once you no longer need it"
elif [ "$stopping" = 0 ] && kill -0 "$pid" 2>/dev/null; then
  # leave AOF off so Redis keeps its RDB saves, and retry the switch on the next start
  cli config set appendonly no >/dev/null 2>&1
  log "switching to AOF failed; serving with AOF off (RDB saves only), the next start tries again"
  # a rewrite Redis retried may have written the manifest just before AOF was turned off; Redis
  # holds no file in the AOF directory once AOF is off, so it moves aside right away
  if manifest_exists; then
    move_stale_aof
  fi
elif [ "$stopping" = 0 ]; then
  log "Redis exited during the switch to AOF; the Redis log lines above give the cause, the next start tries again"
fi
# run until Redis exits and exit with its status. wait still returns the status when the shell
# already reaped Redis while running another command; a forwarded signal interrupts wait, so it
# waits again, and once the status was collected wait returns 127.
status=""
while :; do
  caught=0
  wait "$pid"
  rc=$?
  if [ "$rc" = 127 ] && [ -n "$status" ]; then
    break
  fi
  status=$rc
  [ "$caught" = 1 ] || break
done
log "Redis exited ($status)"
# a clean exit means the shutdown saved the RDB file, which then holds writes the AOF lacks
if [ "$aof_stale" = 1 ] && [ "$status" = 0 ]; then
  move_stale_aof
fi
exit "$status"
