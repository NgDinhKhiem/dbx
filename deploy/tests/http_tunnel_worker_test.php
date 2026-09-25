<?php
declare(strict_types=1);

define('DBX_TUNNEL_FUNCTIONS_ONLY', true);
require dirname(__DIR__) . DIRECTORY_SEPARATOR . 'dbx_tunnel.php';

assert_same(10000, worker_poll_timeout_us(0), 'active polling starts at 10ms');
assert_same(10000, worker_poll_timeout_us(99), 'active polling lasts for 100 idle polls');
assert_same(50000, worker_poll_timeout_us(100), 'brief inactivity backs off to 50ms');
assert_same(50000, worker_poll_timeout_us(119), 'warm polling lasts for 20 idle polls');
assert_same(200000, worker_poll_timeout_us(120), 'idle polling returns to 200ms');
assert_same(0, next_worker_idle_poll_count(120, true), 'activity resets the idle counter');
assert_same(10000, worker_poll_timeout_us(next_worker_idle_poll_count(120, true)), 'activity resets polling to 10ms');
assert_same(120, next_worker_idle_poll_count(120, false), 'the idle counter remains capped');

// Target policy helpers.
assert_same(true, is_restricted_ip('127.0.0.1'), 'loopback is restricted');
assert_same(true, is_restricted_ip('169.254.169.254'), 'metadata is restricted');
assert_same(true, is_restricted_ip('::ffff:127.0.0.1'), 'IPv4-mapped loopback is restricted');
assert_same(true, is_restricted_ip('fe80::1'), 'IPv6 link-local is restricted');
assert_same(true, is_restricted_ip('::1'), 'IPv6 loopback is restricted');
assert_same(false, is_restricted_ip('10.0.0.12'), 'private LAN addresses are not restricted');
assert_same(false, is_restricted_ip('2001:db8::1'), 'global IPv6 is not restricted');
$DBX_TUNNEL_ALLOWED_HOSTS = ['mysql.internal:3306', 'pg.internal', '[2001:db8::5]:5432', 'redis.internal:*', '*.db.example.com:5432'];
assert_same(true, match_allow_list('mysql.internal', 3306) !== null, 'host:port entry matches');
assert_same(null, match_allow_list('mysql.internal', 22), 'host:port entry does not match other ports');
assert_same(true, match_allow_list('pg.internal', 5432) !== null, 'host-only entry matches default DB ports');
assert_same(null, match_allow_list('pg.internal', 22), 'host-only entry rejects non-DB ports');
assert_same(true, match_allow_list('2001:db8::5', 5432) !== null, 'bracketed IPv6 entry matches');
assert_same(true, match_allow_list('redis.internal', 16379) !== null, 'host:* entry matches any port');
assert_same(true, match_allow_list('a.db.example.com', 5432) !== null, 'wildcard entry matches subdomain');
assert_same(null, match_allow_list('db.example.com.evil', 5432), 'wildcard entry does not match suffix tricks');
assert_same(null, match_allow_list('other.internal', 3306), 'unlisted host is refused');

$baseDir = sys_get_temp_dir() . DIRECTORY_SEPARATOR . 'dbx-tunnel-test-' . bin2hex(random_bytes(6));
$sessionDir = $baseDir . DIRECTORY_SEPARATOR . 'session123';
$worker = null;
$pipes = [];
$server = null;
$socket = null;

try {
    if (!mkdir($sessionDir, 0700, true) && !is_dir($sessionDir)) {
        throw new RuntimeException('Failed to create test session directory');
    }
    touch($sessionDir . DIRECTORY_SEPARATOR . 'in.queue');
    touch($sessionDir . DIRECTORY_SEPARATOR . 'out.queue');

    $server = stream_socket_server('tcp://127.0.0.1:0', $errno, $errstr);
    if ($server === false) {
        throw new RuntimeException('Failed to start echo target: ' . $errstr);
    }
    $address = stream_socket_get_name($server, false);
    $port = (int) substr((string) strrchr($address, ':'), 1);

    $script = dirname(__DIR__) . DIRECTORY_SEPARATOR . 'dbx_tunnel.php';
    $command = escapeshellarg(PHP_BINARY)
        . ' ' . escapeshellarg($script)
        . ' --dbx-worker ' . escapeshellarg($sessionDir)
        . ' 127.0.0.1 ' . escapeshellarg((string) $port)
        . ' 5';
    $worker = proc_open($command, [
        0 => ['pipe', 'r'],
        1 => ['pipe', 'w'],
        2 => ['pipe', 'w'],
    ], $pipes);
    if (!is_resource($worker)) {
        throw new RuntimeException('Failed to start tunnel worker');
    }
    fclose($pipes[0]);
    stream_set_blocking($pipes[1], false);
    stream_set_blocking($pipes[2], false);

    $socket = @stream_socket_accept($server, 5);
    if ($socket === false) {
        throw new RuntimeException('Tunnel worker did not connect to echo target');
    }
    stream_set_blocking($socket, false);

    $roundTrips = 100;
    $startedAt = microtime(true);
    for ($index = 0; $index < $roundTrips; $index++) {
        $payload = pack('N', $index) . hash('sha256', (string) $index, true);
        append_chunk($sessionDir, 'in', $payload);

        $forwarded = read_exact_with_timeout($socket, strlen($payload), 2.0);
        assert_same($payload, $forwarded, 'TCP target receives ordered payload ' . $index);
        write_all($socket, $forwarded);

        $reply = drain_exact_with_timeout($sessionDir, 'out', strlen($payload), 2.0);
        assert_same($payload, $reply, 'tunnel returns ordered payload ' . $index);
    }
    $elapsed = microtime(true) - $startedAt;
    if ($elapsed >= 10.0) {
        throw new RuntimeException(sprintf(
            'Sequential tunnel round trips took %.3fs; the old fixed wait pattern takes about 20s',
            $elapsed
        ));
    }

    touch($sessionDir . DIRECTORY_SEPARATOR . 'close');
    wait_for_path($sessionDir . DIRECTORY_SEPARATOR . 'closed', 3.0);

    printf("ok - %d sequential round trips in %.3fs; ordered payloads and clean shutdown verified\n", $roundTrips, $elapsed);
} finally {
    if (is_resource($socket)) {
        fclose($socket);
    }
    if (is_resource($server)) {
        fclose($server);
    }
    if (is_resource($worker)) {
        foreach ([1, 2] as $pipeIndex) {
            if (isset($pipes[$pipeIndex]) && is_resource($pipes[$pipeIndex])) {
                fclose($pipes[$pipeIndex]);
            }
        }
        $status = proc_get_status($worker);
        if ($status['running']) {
            proc_terminate($worker);
        }
        proc_close($worker);
    }
    if (is_dir($baseDir)) {
        remove_dir($baseDir);
    }
}

function read_exact_with_timeout($socket, int $length, float $timeoutSeconds): string
{
    $data = '';
    $deadline = microtime(true) + $timeoutSeconds;
    while (strlen($data) < $length) {
        $chunk = fread($socket, $length - strlen($data));
        if ($chunk === false) {
            throw new RuntimeException('Failed to read echo target socket');
        }
        if ($chunk !== '') {
            $data .= $chunk;
            continue;
        }
        if (microtime(true) >= $deadline) {
            throw new RuntimeException('Timed out reading echo target socket');
        }
        usleep(1000);
    }
    return $data;
}

function drain_exact_with_timeout(string $dir, string $name, int $length, float $timeoutSeconds): string
{
    $data = '';
    $deadline = microtime(true) + $timeoutSeconds;
    while (strlen($data) < $length) {
        $data .= drain_chunks($dir, $name);
        if (strlen($data) >= $length) {
            break;
        }
        if (microtime(true) >= $deadline) {
            throw new RuntimeException('Timed out draining tunnel queue');
        }
        usleep(1000);
    }
    return $data;
}

function wait_for_path(string $path, float $timeoutSeconds): void
{
    $deadline = microtime(true) + $timeoutSeconds;
    while (!is_file($path)) {
        if (microtime(true) >= $deadline) {
            throw new RuntimeException('Timed out waiting for ' . $path);
        }
        usleep(10000);
    }
}

function assert_same($expected, $actual, string $message): void
{
    if ($expected !== $actual) {
        throw new RuntimeException($message);
    }
}
