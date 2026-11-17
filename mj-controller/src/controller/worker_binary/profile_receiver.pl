# This program travels with the controller. The profile store remains usable
# by older controllers and needs no change to the installed worker.
use strict;
use warnings;
use Digest::SHA;
use Fcntl qw(:flock F_SETFD);
use File::Copy qw(copy);
use File::Path qw(make_path remove_tree);
use File::Temp qw(tempdir);
use JSON::PP qw(decode_json);

my ($store, $destination) = @ARGV;
$store = "$ENV{HOME}/$store";
# Keep leases and pins on the object filesystem, even when the store itself
# is a separate mount. The legacy collector reserves .stage-* for pinned work.
my $operations = "$store/.stage-negotiated";
umask 0077;
make_path($store, $operations, "$operations/claims");
chmod(0700, $store, $operations) == 2 or die "secure profile store: $!";

# The metadata lock only protects decisions over names. It is never held
# while waiting for another transfer, receiving bytes, or copying a profile.
open(my $metadata, '>>', "$operations/metadata") or die "open profile store lock: $!";
flock($metadata, LOCK_EX) or die "lock profile store: $!";
my $work = tempdir('receive-XXXXXXXXXX', DIR => $operations);
open(my $lease, '>>', "$work/lease") or die "open profile receiver lease: $!";
flock($lease, LOCK_EX) or die "lock profile receiver lease: $!";
# Tar may still be writing after a receiver is killed. Keep the operation
# lease in that child until it exits so collection cannot delete its files.
defined(fcntl($lease, F_SETFD, 0)) or die "share profile lease with tar: $!";
END { remove_tree($work) if defined $work }
mkdir "$work/pins" or die "create profile pins: $!";
flock($metadata, LOCK_UN) or die "unlock profile store: $!";

my @offer;
my %offered;
my $complete;
while (defined(my $line = <STDIN>)) {
    if ($line eq "\n") { $complete = 1; last }
    my ($key, $size) = $line =~ /\A([0-9a-f]{64}) (0|[1-9][0-9]*)\n\z/
        or die "invalid profile offer";
    die "duplicate profile offer" if exists $offered{$key};
    $offered{$key} = $size;
    push @offer, $key;
}
die "incomplete profile offer" unless $complete;

flock($metadata, LOCK_EX) or die "lock profile store: $!";
my (%claims, @reply);
my $busy = 0;
for my $key (@offer) {
    my $pinned = 0;
    for my $name ($key, "$key.x") {
        my $blob = "$store/$name";
        if (-f $blob && -s $blob == $offered{$key} && link($blob, "$work/pins/$key")) {
            $pinned = 1;
            last;
        }
    }
    if ($pinned) {
        utime(undef, undef, "$work/pins/$key") or die "touch profile content: $!";
        push @reply, "present $key\n";
        next;
    }
    open(my $claim, '>>', "$operations/claims/$key") or die "open profile claim: $!";
    if (flock($claim, LOCK_EX | LOCK_NB)) {
        $claims{$key} = $claim;
        push @reply, "claimed $key\n";
    } elsif ($!{EWOULDBLOCK}) {
        $busy = 1;
        push @reply, "busy $key\n";
    } else {
        die "claim profile content: $!";
    }
}

# Match the legacy store's collection schedule and retention. Live operation
# pins survive eviction of their store names (including by older controllers).
if ((! -f "$store/.collected") || -M _ > 1 / 24) {
    opendir(my $blobs, $store) or die "list profile objects: $!";
    for my $name (readdir($blobs)) {
        next unless $name =~ /\A[0-9a-f]{64}(?:\.x)?\z/;
        my $blob = "$store/$name";
        if (-f $blob && -M _ > 1) {
            unlink($blob) or $!{ENOENT} or die "collect profile object: $!";
        }
    }
    open(my $stamp, '>', "$store/.collected") or die "mark profile store collection: $!";
}
# Reap crashed receivers and expired unowned claim names on the same schedule
# as profile objects. Never unlink a lock file with a live holder.
if ((! -f "$operations/collected") || -M _ > 1 / 24) {
    opendir(my $dirs, $operations) or die "list profile operations: $!";
    for my $name (readdir($dirs)) {
        next unless $name =~ /\Areceive-[a-zA-Z0-9_]+\z/ && "$operations/$name" ne $work;
        my $directory = "$operations/$name";
        my $old;
        if (! -e "$directory/lease") {
            remove_tree($directory, {safe => 1});
        } elsif (open($old, '<', "$directory/lease") && flock($old, LOCK_EX | LOCK_NB)) {
            remove_tree($directory, {safe => 1});
        }
    }
    opendir(my $locks, "$operations/claims") or die "list profile claims: $!";
    for my $name (readdir($locks)) {
        next unless $name =~ /\A[0-9a-f]{64}(?:\.x)?\z/;
        my $path = "$operations/claims/$name";
        next unless -M $path > 1;
        my $old;
        if (open($old, '<', $path) && flock($old, LOCK_EX | LOCK_NB)) {
            unlink($path) or die "collect profile claim: $!";
        }
    }
    open(my $stamp, '>', "$operations/collected") or die "mark profile operation collection: $!";
}
flock($metadata, LOCK_UN) or die "unlock profile store: $!";

$| = 1;
print @reply, "\n";
exit 0 if !%claims && ($busy || !defined($destination));
# The client sends nothing else until this reply, so its next bytes have
# never entered Perl's line buffer and tar can read directly from stdin.
mkdir "$work/incoming" or die "create profile archive staging: $!";
system('tar', '-xzf', '-', '--no-same-owner', '--no-same-permissions', '-C', "$work/incoming") == 0
    or die "extract negotiated profile archive";
for my $key (sort keys %claims) {
    my $blob = "$work/incoming/objects/$key";
    open(my $file, '<:raw', $blob) or die "read claimed profile object: $!";
    die "profile object length mismatch: $key" unless -s $file == $offered{$key};
    my $digest = Digest::SHA->new(256)->addfile($file)->hexdigest;
    die "profile object digest mismatch: $key" unless $digest eq substr($key, 0, 64);
    chmod(0600, $blob) or die "protect profile object: $!";
    # Pin the verified inode before publication. Collection or a compatible
    # older controller can change the store name, never this operation's pin.
    link($blob, "$work/pins/$key") or die "pin received profile object: $!";
    rename($blob, "$store/$key") or die "publish profile object: $!";
    delete($claims{$key});
}
exit 0 if $busy || !defined($destination);
open(my $manifest, '<:raw', "$work/incoming/manifest.json") or die "read profile manifest: $!";
my $entries = do { local $/; decode_json(<$manifest>) };
my $tree = "$work/profile";
mkdir $tree or die "create materialized profile: $!";
my @directories;
for my $entry (@$entries) {
    my $path = pack('H*', $entry->{path});
    die "invalid profile path" if $path =~ m{(?:\A/|\0|(?:\A|/)\.\.?(/|\z))};
    my $target = length($path) ? "$tree/$path" : $tree;
    if ($entry->{directory}) {
        mkdir($target) or die "create profile directory: $!" if length($path);
        push @directories, [$target, $entry->{mode}];
    } else {
        my $key = $entry->{key};
        die "unoffered profile object" unless exists $offered{$key};
        die "inconsistent profile entry size" unless $entry->{size} == $offered{$key};
        copy("$work/pins/$key", $target) or die "materialize profile object: $!";
        chmod($entry->{mode}, $target) or die "set materialized profile mode: $!";
    }
}
if (-f "$work/incoming/private.tar.gz") {
    system('tar', '-xzf', "$work/incoming/private.tar.gz", '--no-same-owner',
        '--no-same-permissions', '-C', $tree) == 0 or die "extract private profile bundle";
}
for my $directory (reverse @directories) {
    chmod($directory->[1], $directory->[0]) or die "set profile directory mode: $!";
}
# The caller uses a session-specific staging destination and only copies it
# into the home after success. Copying here supports a different filesystem.
remove_tree($destination);
system('cp', '-pRP', '--', $tree, $destination) == 0 or die "stage materialized profile";
