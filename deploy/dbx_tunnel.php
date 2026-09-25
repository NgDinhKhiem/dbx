<?php
declare(strict_types=1);

/*
 * DBX HTTP Script Tunnel
 *
 * Upload this file to a PHP server that can reach the target database.
 * Every setting below is read from the environment; you may instead hard-code
 * a value by editing the matching $DBX_TUNNEL_* assignment further down.
 *
 * Required configuration:
 *   DBX_TUNNEL_TOKEN=change-this-to-a-long-random-string
 *
 * Required target policy (one of the two):
 *   DBX_TUNNEL_ALLOWED_HOSTS=mysql.internal:3306,10.0.0.12:5432,pg.internal
 *       Comma-separated allow-list. Entry forms:
 *         host:port        exactly that host and port
 *         host:*           that host on any port
 *         host             that host on well-known database ports only
 *                          (see DBX_TUNNEL_DEFAULT_PORTS)
 *         [v6addr]:port    IPv6 literal with a port
 *         *.example.com:p  any subdomain of example.com on port p
 *       Hosts are compared literally (case-insensitive); the script never
 *       expands an entry to other names.
 *       DBX_TUNNEL_ALLOWED_TARGETS is accepted as an alias and merged in.
 *   DBX_TUNNEL_ALLOW_ANY_TARGET=true
 *       Allow any host:port. Only use this when the PHP host itself sits in a
 *       network where every reachable service is acceptable to expose to
 *       token holders. Loopback/link-local/metadata addresses stay blocked.
 *   With neither set, every "open" is refused (older versions allowed all).
 *
 * Loopback (127.0.0.0/8, ::1), link-local (169.254.0.0/16, fe80::/10),
 * unspecified, multicast and cloud metadata addresses are refused even when
 * the target host name is allowed, unless the resolved IP itself is listed
 * (e.g. "127.0.0.1:3306") or the matching entry is "localhost". Host names are
 * resolved once and the worker connects to the checked IP, so DNS rebinding
 * cannot redirect an allowed name to an internal address.
 *
 * Transport:
 *   DBX_TUNNEL_ALLOW_INSECURE_HTTP=false  Plain-HTTP requests are refused
 *       unless this is true. HTTPS is detected via $_SERVER['HTTPS'],
 *       REQUEST_SCHEME, or X-Forwarded-Proto from a trusted proxy.
 *   DBX_TUNNEL_TRUSTED_PROXIES=           Comma-separated proxy IPs/CIDRs
 *       whose X-Forwarded-Proto header is trusted (e.g. 127.0.0.1,10.0.0.0/8).
 *
 * Optional configuration:
 *   DBX_TUNNEL_DIR=<sys temp>/dbx_tunnel_<uid>  Session queue directory. Must be
 *       owned by the PHP user; it is forced to mode 0700.
 *   DBX_TUNNEL_MAX_SESSION_SECONDS=3600  Hard lifetime of one tunnel session.
 *   DBX_TUNNEL_IDLE_TIMEOUT_SECONDS=120  Worker exits when the DBX client has
 *       not polled or written for this long.
 *   DBX_TUNNEL_MAX_SESSIONS=16           Concurrent sessions. With PHP-FPM each
 *       session holds one FPM worker, so keep this below pm.max_children.
 *   DBX_TUNNEL_MAX_WRITE_BYTES=1048576   Maximum body of one "write" request.
 *   DBX_TUNNEL_MAX_QUEUE_BYTES=16777216  Maximum buffered bytes per direction.
 *   DBX_TUNNEL_DEFAULT_PORTS=<list>      Ports allowed for host-only entries.
 *   DBX_TUNNEL_PHP=/usr/bin/php          PHP CLI used when fastcgi_finish_request
 *       is unavailable.
 *
 * Error responses are deliberately generic; details go to the PHP error log.
 */

$DBX_TUNNEL_TOKEN = getenv('DBX_TUNNEL_TOKEN') ?: '';
$DBX_TUNNEL_DIR = getenv('DBX_TUNNEL_DIR') ?: default_tunnel_dir();
$DBX_TUNNEL_ALLOWED_HOSTS = array_values(array_unique(array_merge(
    env_list('DBX_TUNNEL_ALLOWED_HOSTS'),
    env_list('DBX_TUNNEL_ALLOWED_TARGETS')
)));
$DBX_TUNNEL_ALLOW_ANY_TARGET = env_bool('DBX_TUNNEL_ALLOW_ANY_TARGET', false);
$DBX_TUNNEL_ALLOW_INSECURE_HTTP = env_bool('DBX_TUNNEL_ALLOW_INSECURE_HTTP', false);
$DBX_TUNNEL_TRUSTED_PROXIES = env_list('DBX_TUNNEL_TRUSTED_PROXIES');
$DBX_TUNNEL_MAX_SESSION_SECONDS = max(30, (int) (getenv('DBX_TUNNEL_MAX_SESSION_SECONDS') ?: '3600'));
$DBX_TUNNEL_IDLE_TIMEOUT_SECONDS = max(10, (int) (getenv('DBX_TUNNEL_IDLE_TIMEOUT_SECONDS') ?: '120'));
$DBX_TUNNEL_MAX_SESSIONS = max(1, (int) (getenv('DBX_TUNNEL_MAX_SESSIONS') ?: '16'));
$DBX_TUNNEL_MAX_WRITE_BYTES = max(65536, (int) (getenv('DBX_TUNNEL_MAX_WRITE_BYTES') ?: '1048576'));
$DBX_TUNNEL_MAX_QUEUE_BYTES = max(1048576, (int) (getenv('DBX_TUNNEL_MAX_QUEUE_BYTES') ?: '16777216'));
$DBX_TUNNEL_DEFAULT_PORTS = array_values(array_filter(array_map('intval', env_list('DBX_TUNNEL_DEFAULT_PORTS') ?: [
    // MySQL/MariaDB, PostgreSQL, SQL Server, Oracle, MongoDB, Redis, ClickHouse,
    // Elasticsearch, Dameng, KingbaseES, DB2, CockroachDB, TiDB, Cassandra,
    // StarRocks/Doris, Firebird, OceanBase, SAP HANA, Redshift, GaussDB, IRIS.
    '3306', '5432', '1433', '1521', '27017', '6379', '8123', '9000', '9200',
    '5236', '54321', '50000', '26257', '4000', '9042', '9030', '3050', '2881',
    '2883', '30015', '5439', '5866', '1972',
])));

// Keep sequential protocol exchanges responsive, then restore the original idle cadence.
const DBX_WORKER_ACTIVE_POLL_US = 10000;
const DBX_WORKER_WARM_POLL_US = 50000;
const DBX_WORKER_IDLE_POLL_US = 200000;
const DBX_WORKER_ACTIVE_POLL_COUNT = 100;
const DBX_WORKER_WARM_POLL_COUNT = 20;

// Addresses that are never reachable through the tunnel unless explicitly listed.
const DBX_RESTRICTED_CIDRS = [
    '0.0.0.0/8',          // "this" network / unspecified
    '127.0.0.0/8',        // loopback
    '169.254.0.0/16',     // link-local, including 169.254.169.254 metadata
    '100.100.100.200/32', // Alibaba Cloud metadata
    '168.63.129.16/32',   // Azure wireserver
    '224.0.0.0/4',        // multicast
    '240.0.0.0/4',        // reserved + broadcast
    '::/128',             // unspecified
    '::1/128',            // loopback
    'fe80::/10',          // link-local
    'fd00:ec2::254/128',  // AWS IMDS over IPv6
    'ff00::/8',           // multicast
];

// Standalone tests load the worker helpers without dispatching an HTTP request.
if (defined('DBX_TUNNEL_FUNCTIONS_ONLY') && DBX_TUNNEL_FUNCTIONS_ONLY) {
    return;
}

if (PHP_SAPI === 'cli' && isset($argv[1]) && $argv[1] === '--dbx-worker') {
    // Limits are passed explicitly because PHP-FPM usually clears the environment of spawned children.
    if (isset($argv[6])) {
        $DBX_TUNNEL_MAX_SESSION_SECONDS = max(30, (int) $argv[6]);
    }
    if (isset($argv[7])) {
        $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS = max(10, (int) $argv[7]);
    }
    if (isset($argv[8])) {
        $DBX_TUNNEL_MAX_QUEUE_BYTES = max(1048576, (int) $argv[8]);
    }
    run_worker($argv[2] ?? '', $argv[3] ?? '', (int) ($argv[4] ?? 0), (int) ($argv[5] ?? 10));
    exit;
}

try {
    require_https();
    require_token();
    ensure_base_dir();
    cleanup_old_sessions();

    $action = request_param('dbx_action');
    switch ($action) {
        case 'open':
            handle_open();
            break;
        case 'write':
            handle_write();
            break;
        case 'read':
            handle_read();
            break;
        case 'close':
            handle_close();
            break;
        default:
            respond_error(400, 'Unsupported dbx_action');
    }
} catch (Throwable $e) {
    internal_error('Internal tunnel error', get_class($e) . ': ' . $e->getMessage());
}

function handle_open(): void
{
    global $DBX_TUNNEL_MAX_SESSIONS;

    $session = request_session();
    $host = request_param('dbx_target_host');
    $port = (int) request_param('dbx_target_port');
    $connectTimeout = request_connect_timeout();
    $connectIp = validate_target($host, $port);

    $dir = session_dir($session);
    $lock = open_lock(base_dir() . DIRECTORY_SEPARATOR . '.open.lock');
    try {
        if (count_active_sessions() >= $DBX_TUNNEL_MAX_SESSIONS) {
            log_detail('refusing open: ' . $DBX_TUNNEL_MAX_SESSIONS . ' sessions already active');
            respond_error(503, 'Too many active tunnel sessions');
        }
        // A non-recursive mkdir is atomic: an existing session id is never re-opened.
        $old = umask(0077);
        $created = @mkdir($dir, 0700);
        umask($old);
        if (!$created) {
            if (file_exists($dir)) {
                respond_error(409, 'Tunnel session already exists');
            }
            internal_error('Failed to create tunnel session', 'mkdir failed for ' . $dir);
        }
        file_put_contents($dir . DIRECTORY_SEPARATOR . 'meta.json', json_encode([
            'target_host' => $host,
            'target_ip' => $connectIp,
            'target_port' => $port,
            'created_at' => time(),
        ]));
        touch($dir . DIRECTORY_SEPARATOR . 'in.queue');
        touch($dir . DIRECTORY_SEPARATOR . 'out.queue');
        touch($dir . DIRECTORY_SEPARATOR . 'heartbeat');
    } finally {
        release_lock($lock);
    }

    if (function_exists('fastcgi_finish_request')) {
        respond_json(['ok' => true], false);
        fastcgi_finish_request();
        run_worker($dir, $connectIp, $port, $connectTimeout);
        exit;
    }

    if (spawn_worker($dir, $connectIp, $port, $connectTimeout)) {
        if (wait_for_worker_start($dir, 2000)) {
            respond_json(['ok' => true]);
        }
        mark_closed($dir);
        internal_error('PHP tunnel worker did not start', 'worker did not create worker.started in ' . $dir);
    }

    mark_closed($dir);
    internal_error(
        'PHP tunnel worker is unavailable',
        'PHP tunnel requires PHP-FPM fastcgi_finish_request or permission to spawn PHP CLI'
    );
}

function handle_write(): void
{
    global $DBX_TUNNEL_MAX_WRITE_BYTES, $DBX_TUNNEL_MAX_QUEUE_BYTES;

    $dir = existing_session_dir(request_session());
    ensure_session_alive($dir);
    touch_heartbeat($dir);

    $declared = $_SERVER['CONTENT_LENGTH'] ?? '';
    if ($declared !== '' && is_digits((string) $declared) && (int) $declared > $DBX_TUNNEL_MAX_WRITE_BYTES) {
        respond_error(413, 'Tunnel write is too large');
    }
    $data = file_get_contents('php://input', false, null, 0, $DBX_TUNNEL_MAX_WRITE_BYTES + 1);
    if ($data !== false && strlen($data) > $DBX_TUNNEL_MAX_WRITE_BYTES) {
        respond_error(413, 'Tunnel write is too large');
    }
    if ($data !== false && $data !== '') {
        if (queue_size($dir, 'in') + strlen($data) * 4 / 3 > $DBX_TUNNEL_MAX_QUEUE_BYTES) {
            respond_error(503, 'Tunnel session buffer is full');
        }
        append_chunk($dir, 'in', $data);
    }
    respond_json(['ok' => true]);
}

function handle_read(): void
{
    $dir = existing_session_dir(request_session());
    touch_heartbeat($dir);
    $waitMs = max(0, min(5000, (int) (request_param('dbx_wait_ms', '1000'))));
    $deadline = microtime(true) + ($waitMs / 1000);

    do {
        $data = drain_chunks($dir, 'out');
        if ($data !== '') {
            header('Content-Type: application/octet-stream');
            header('Cache-Control: no-store');
            echo $data;
            return;
        }
        if (is_file($dir . DIRECTORY_SEPARATOR . 'error.txt')) {
            respond_error(502, trim((string) file_get_contents($dir . DIRECTORY_SEPARATOR . 'error.txt')));
        }
        if (is_file($dir . DIRECTORY_SEPARATOR . 'closed')) {
            respond_error(410, 'Tunnel session closed');
        }
        usleep(20000);
    } while (microtime(true) < $deadline);

    http_response_code(204);
}

function handle_close(): void
{
    $dir = existing_session_dir(request_session());
    touch($dir . DIRECTORY_SEPARATOR . 'close');
    respond_json(['ok' => true]);
}

function run_worker(string $dir, string $host, int $port, int $connectTimeout): void
{
    global $DBX_TUNNEL_MAX_SESSION_SECONDS, $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS, $DBX_TUNNEL_MAX_QUEUE_BYTES;

    if ($dir === '' || $host === '' || $port <= 0) {
        return;
    }
    ignore_user_abort(true);
    set_time_limit(0);
    @touch($dir . DIRECTORY_SEPARATOR . 'worker.started');

    $targetHost = strpos($host, ':') !== false && substr($host, 0, 1) !== '[' ? '[' . $host . ']' : $host;
    $socket = @stream_socket_client(
        'tcp://' . $targetHost . ':' . $port,
        $errno,
        $errstr,
        max(1, min(300, $connectTimeout)),
        STREAM_CLIENT_CONNECT
    );
    if (!$socket) {
        log_detail(sprintf('connect to %s:%d failed: [%d] %s', $host, $port, (int) $errno, (string) $errstr));
        write_error($dir, 'Failed to connect target database');
        mark_closed($dir);
        return;
    }

    stream_set_blocking($socket, false);
    $startedAt = time();
    $expiresAt = $startedAt + $DBX_TUNNEL_MAX_SESSION_SECONDS;
    $lastActivity = $startedAt;
    $lastHeartbeatCheck = 0;
    $idlePolls = 0;

    try {
        while (time() < $expiresAt) {
            if (is_file($dir . DIRECTORY_SEPARATOR . 'close')) {
                break;
            }

            // The DBX client polls "read" continuously; a silent client means it is gone.
            $now = time();
            if ($now !== $lastHeartbeatCheck) {
                $lastHeartbeatCheck = $now;
                if ($now - max($startedAt, session_last_seen($dir)) > $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS) {
                    log_detail('closing idle tunnel session ' . basename($dir));
                    break;
                }
            }

            $activity = false;
            $inbound = drain_chunks($dir, 'in');
            if ($inbound !== '') {
                write_all($socket, $inbound);
                $lastActivity = time();
                $activity = true;
            }

            // Back-pressure: stop reading from the target while the client is not draining.
            if (queue_size($dir, 'out') >= $DBX_TUNNEL_MAX_QUEUE_BYTES) {
                usleep(worker_poll_timeout_us($idlePolls));
                continue;
            }

            $read = [$socket];
            $write = [];
            $except = [];
            $ready = @stream_select($read, $write, $except, 0, worker_poll_timeout_us($idlePolls));
            if ($ready === false) {
                write_error($dir, 'Failed to poll target database socket');
                break;
            }
            if ($ready > 0) {
                $data = fread($socket, 16384);
                if ($data === false) {
                    write_error($dir, 'Failed to read target database socket');
                    break;
                }
                if ($data === '') {
                    if (feof($socket)) {
                        break;
                    }
                } else {
                    append_chunk($dir, 'out', $data);
                    $lastActivity = time();
                    $activity = true;
                }
            }

            $idlePolls = next_worker_idle_poll_count($idlePolls, $activity);

            if (time() - $lastActivity > $DBX_TUNNEL_MAX_SESSION_SECONDS) {
                break;
            }
        }
    } catch (Throwable $e) {
        log_detail('tunnel worker failed: ' . $e->getMessage());
        write_error($dir, 'Tunnel worker failed');
    }

    fclose($socket);
    mark_closed($dir);
}

function worker_poll_timeout_us(int $idlePolls): int
{
    if ($idlePolls < DBX_WORKER_ACTIVE_POLL_COUNT) {
        return DBX_WORKER_ACTIVE_POLL_US;
    }
    if ($idlePolls < DBX_WORKER_ACTIVE_POLL_COUNT + DBX_WORKER_WARM_POLL_COUNT) {
        return DBX_WORKER_WARM_POLL_US;
    }
    return DBX_WORKER_IDLE_POLL_US;
}

function next_worker_idle_poll_count(int $idlePolls, bool $activity): int
{
    if ($activity) {
        return 0;
    }
    return min($idlePolls + 1, DBX_WORKER_ACTIVE_POLL_COUNT + DBX_WORKER_WARM_POLL_COUNT);
}

function write_all($socket, string $data): void
{
    $offset = 0;
    $length = strlen($data);
    while ($offset < $length) {
        $written = fwrite($socket, substr($data, $offset));
        if ($written === false) {
            throw new RuntimeException('Failed to write target database socket');
        }
        if ($written === 0) {
            usleep(10000);
            continue;
        }
        $offset += $written;
    }
}

function spawn_worker(string $dir, string $host, int $port, int $connectTimeout): bool
{
    global $DBX_TUNNEL_MAX_SESSION_SECONDS, $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS, $DBX_TUNNEL_MAX_QUEUE_BYTES;

    if (!function_exists('popen')) {
        return false;
    }
    $php = getenv('DBX_TUNNEL_PHP') ?: PHP_BINARY;
    if ($php === '') {
        return false;
    }
    $cmd = escapeshellarg($php)
        . ' ' . escapeshellarg(__FILE__)
        . ' --dbx-worker ' . escapeshellarg($dir)
        . ' ' . escapeshellarg($host)
        . ' ' . escapeshellarg((string) $port)
        . ' ' . escapeshellarg((string) $connectTimeout)
        . ' ' . escapeshellarg((string) $DBX_TUNNEL_MAX_SESSION_SECONDS)
        . ' ' . escapeshellarg((string) $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS)
        . ' ' . escapeshellarg((string) $DBX_TUNNEL_MAX_QUEUE_BYTES)
        . ' > /dev/null 2>&1 &';
    $handle = @popen($cmd, 'r');
    if (!is_resource($handle)) {
        return false;
    }
    @pclose($handle);
    return true;
}

function wait_for_worker_start(string $dir, int $timeoutMs): bool
{
    $deadline = microtime(true) + ($timeoutMs / 1000);
    do {
        if (is_file($dir . DIRECTORY_SEPARATOR . 'worker.started')) {
            return true;
        }
        usleep(20000);
    } while (microtime(true) < $deadline);
    return false;
}

function append_chunk(string $dir, string $name, string $data): void
{
    if ($data === '') {
        return;
    }
    $lock = fopen($dir . DIRECTORY_SEPARATOR . $name . '.lock', 'c');
    if (!$lock) {
        throw new RuntimeException('Failed to open tunnel queue lock');
    }
    flock($lock, LOCK_EX);
    file_put_contents($dir . DIRECTORY_SEPARATOR . $name . '.queue', base64_encode($data) . "\n", FILE_APPEND | LOCK_EX);
    flock($lock, LOCK_UN);
    fclose($lock);
}

function drain_chunks(string $dir, string $name): string
{
    $path = $dir . DIRECTORY_SEPARATOR . $name . '.queue';
    if (!is_file($path)) {
        return '';
    }
    $lock = fopen($dir . DIRECTORY_SEPARATOR . $name . '.lock', 'c');
    if (!$lock) {
        throw new RuntimeException('Failed to open tunnel queue lock');
    }
    flock($lock, LOCK_EX);
    $encoded = (string) file_get_contents($path);
    file_put_contents($path, '');
    flock($lock, LOCK_UN);
    fclose($lock);

    $decoded = '';
    foreach (explode("\n", $encoded) as $line) {
        if ($line === '') {
            continue;
        }
        $chunk = base64_decode($line, true);
        if ($chunk !== false) {
            $decoded .= $chunk;
        }
    }
    return $decoded;
}

function queue_size(string $dir, string $name): int
{
    $path = $dir . DIRECTORY_SEPARATOR . $name . '.queue';
    clearstatcache(true, $path);
    $size = @filesize($path);
    return $size === false ? 0 : $size;
}

function require_https(): void
{
    global $DBX_TUNNEL_ALLOW_INSECURE_HTTP;

    if ($DBX_TUNNEL_ALLOW_INSECURE_HTTP || request_is_https()) {
        return;
    }
    respond_error(403, 'HTTPS is required');
}

function request_is_https(): bool
{
    $https = strtolower((string) ($_SERVER['HTTPS'] ?? ''));
    if ($https !== '' && $https !== 'off') {
        return true;
    }
    if (strtolower((string) ($_SERVER['REQUEST_SCHEME'] ?? '')) === 'https') {
        return true;
    }
    if (isset($_SERVER['HTTP_X_FORWARDED_PROTO']) && remote_is_trusted_proxy()) {
        // The last hop was appended by the trusted proxy itself.
        $hops = explode(',', (string) $_SERVER['HTTP_X_FORWARDED_PROTO']);
        return strtolower(trim((string) end($hops))) === 'https';
    }
    return false;
}

function remote_is_trusted_proxy(): bool
{
    global $DBX_TUNNEL_TRUSTED_PROXIES;

    $remote = (string) ($_SERVER['REMOTE_ADDR'] ?? '');
    $remoteBin = $remote !== '' ? @inet_pton($remote) : false;
    if ($remoteBin === false) {
        return false;
    }
    foreach ($DBX_TUNNEL_TRUSTED_PROXIES as $proxy) {
        $cidr = strpos($proxy, '/') === false ? $proxy . (strpos($proxy, ':') === false ? '/32' : '/128') : $proxy;
        if (ip_in_cidr($remoteBin, $cidr)) {
            return true;
        }
    }
    return false;
}

function require_token(): void
{
    global $DBX_TUNNEL_TOKEN;

    if ($DBX_TUNNEL_TOKEN === '') {
        log_detail('DBX_TUNNEL_TOKEN is not configured; refusing all requests');
        respond_error(503, 'Service unavailable');
    }
    $provided = request_token();
    if ($provided === '' || !hash_equals($DBX_TUNNEL_TOKEN, $provided)) {
        respond_error(401, 'Invalid tunnel token');
    }
}

function request_token(): string
{
    if (isset($_SERVER['HTTP_X_DBX_TUNNEL_TOKEN'])) {
        return trim((string) $_SERVER['HTTP_X_DBX_TUNNEL_TOKEN']);
    }
    $authorization = $_SERVER['HTTP_AUTHORIZATION'] ?? $_SERVER['REDIRECT_HTTP_AUTHORIZATION'] ?? '';
    if (stripos($authorization, 'Bearer ') === 0) {
        return trim(substr($authorization, 7));
    }
    return '';
}

/**
 * Validates the requested target against the policy and returns the IP address
 * the worker must connect to (resolved once, so DNS cannot change it later).
 */
function validate_target(string $host, int $port): string
{
    global $DBX_TUNNEL_ALLOWED_HOSTS, $DBX_TUNNEL_ALLOW_ANY_TARGET;

    if ($host === '' || preg_match('/[\x00-\x20]/', $host)) {
        respond_error(400, 'Invalid target host');
    }
    $normalized = normalize_host($host);
    if (
        !filter_var($normalized, FILTER_VALIDATE_IP)
        && !filter_var($normalized, FILTER_VALIDATE_DOMAIN, FILTER_FLAG_HOSTNAME)
    ) {
        respond_error(400, 'Invalid target host');
    }
    if ($port < 1 || $port > 65535) {
        respond_error(400, 'Invalid target port');
    }

    if ($DBX_TUNNEL_ALLOWED_HOSTS === [] && !$DBX_TUNNEL_ALLOW_ANY_TARGET) {
        log_detail('no target policy configured; set DBX_TUNNEL_ALLOWED_HOSTS or DBX_TUNNEL_ALLOW_ANY_TARGET');
        respond_error(403, 'No tunnel targets are allowed on this server');
    }
    $entry = match_allow_list($normalized, $port);
    if ($entry === null && !$DBX_TUNNEL_ALLOW_ANY_TARGET) {
        respond_error(403, 'Target is not allowed');
    }

    $ips = resolve_ips($normalized);
    if ($ips === []) {
        log_detail('failed to resolve target host ' . $normalized);
        respond_error(502, 'Failed to resolve target host');
    }
    foreach ($ips as $ip) {
        if (!is_restricted_ip($ip)) {
            continue;
        }
        $explicit = match_allow_list($ip, $port) !== null
            || ($entry !== null && $entry['host'] === 'localhost' && is_loopback_ip($ip));
        if (!$explicit) {
            log_detail(sprintf('refusing restricted target %s (%s):%d', $normalized, $ip, $port));
            respond_error(403, 'Target is not allowed');
        }
    }
    return $ips[0];
}

function normalize_host(string $host): string
{
    $host = strtolower(trim($host));
    if (strlen($host) > 1 && $host[0] === '[' && substr($host, -1) === ']') {
        $host = substr($host, 1, -1);
    }
    $host = rtrim($host, '.');
    if (filter_var($host, FILTER_VALIDATE_IP)) {
        $bin = @inet_pton($host);
        if ($bin !== false) {
            $host = (string) inet_ntop($bin);
        }
    }
    return $host;
}

/** @return array{host: string, port: int|string|null}|null */
function parse_allow_entry(string $raw): ?array
{
    $raw = strtolower(trim($raw));
    if ($raw === '') {
        return null;
    }
    $portPart = null;
    if ($raw[0] === '[') {
        $end = strpos($raw, ']');
        if ($end === false) {
            return null;
        }
        $host = substr($raw, 1, $end - 1);
        $rest = substr($raw, $end + 1);
        if ($rest !== '') {
            if ($rest[0] !== ':') {
                return null;
            }
            $portPart = substr($rest, 1);
        }
    } elseif (substr_count($raw, ':') === 1) {
        [$host, $portPart] = explode(':', $raw, 2);
    } else {
        $host = $raw; // host name, IPv4, or bare IPv6 without a port
    }
    if ($portPart === null) {
        $port = null;
    } elseif ($portPart === '*') {
        $port = '*';
    } elseif (is_digits($portPart) && (int) $portPart >= 1 && (int) $portPart <= 65535) {
        $port = (int) $portPart;
    } else {
        return null;
    }
    $wildcard = strpos($host, '*.') === 0;
    $host = normalize_host($wildcard ? substr($host, 2) : $host);
    if ($host === '') {
        return null;
    }
    return ['host' => $wildcard ? '*.' . $host : $host, 'port' => $port];
}

/** @return array{host: string, port: int|string|null}|null */
function match_allow_list(string $host, int $port): ?array
{
    global $DBX_TUNNEL_ALLOWED_HOSTS, $DBX_TUNNEL_DEFAULT_PORTS;

    foreach ($DBX_TUNNEL_ALLOWED_HOSTS as $raw) {
        $entry = parse_allow_entry($raw);
        if ($entry === null) {
            log_detail('ignoring invalid allow-list entry: ' . $raw);
            continue;
        }
        $hostMatches = $entry['host'] === $host
            || (strpos($entry['host'], '*.') === 0 && substr($host, -strlen($entry['host']) + 1) === substr($entry['host'], 1));
        if (!$hostMatches) {
            continue;
        }
        if (
            $entry['port'] === '*'
            || $entry['port'] === $port
            || ($entry['port'] === null && in_array($port, $DBX_TUNNEL_DEFAULT_PORTS, true))
        ) {
            return $entry;
        }
    }
    return null;
}

/** @return string[] */
function resolve_ips(string $host): array
{
    if (filter_var($host, FILTER_VALIDATE_IP)) {
        return [$host];
    }
    $ips = [];
    $v4 = @gethostbynamel($host);
    if (is_array($v4)) {
        $ips = $v4;
    }
    if ($ips === [] && function_exists('dns_get_record')) {
        $records = @dns_get_record($host, DNS_AAAA);
        foreach (is_array($records) ? $records : [] as $record) {
            if (isset($record['ipv6'])) {
                $ips[] = normalize_host((string) $record['ipv6']);
            }
        }
    }
    return array_values(array_unique(array_filter($ips, static function ($ip): bool {
        return (bool) filter_var($ip, FILTER_VALIDATE_IP);
    })));
}

function is_restricted_ip(string $ip): bool
{
    $bin = @inet_pton($ip);
    if ($bin === false) {
        return true;
    }
    if (strlen($bin) === 16) {
        // IPv4-mapped (::ffff:0:0/96) and NAT64 (64:ff9b::/96) embed an IPv4 target.
        if (ip_in_cidr($bin, '::ffff:0:0/96') || ip_in_cidr($bin, '64:ff9b::/96')) {
            return is_restricted_ip((string) inet_ntop(substr($bin, 12)));
        }
    }
    foreach (DBX_RESTRICTED_CIDRS as $cidr) {
        if (ip_in_cidr($bin, $cidr)) {
            return true;
        }
    }
    return false;
}

function is_loopback_ip(string $ip): bool
{
    $bin = @inet_pton($ip);
    if ($bin === false) {
        return false;
    }
    if (strlen($bin) === 16 && ip_in_cidr($bin, '::ffff:0:0/96')) {
        $bin = substr($bin, 12);
    }
    return ip_in_cidr($bin, '127.0.0.0/8') || ip_in_cidr($bin, '::1/128');
}

function ip_in_cidr(string $ipBin, string $cidr): bool
{
    $parts = explode('/', $cidr, 2);
    $netBin = @inet_pton($parts[0]);
    if ($netBin === false || strlen($netBin) !== strlen($ipBin)) {
        return false;
    }
    $bits = isset($parts[1]) && is_digits($parts[1]) ? (int) $parts[1] : strlen($netBin) * 8;
    $bits = max(0, min(strlen($netBin) * 8, $bits));
    $fullBytes = intdiv($bits, 8);
    if (substr($ipBin, 0, $fullBytes) !== substr($netBin, 0, $fullBytes)) {
        return false;
    }
    $remaining = $bits % 8;
    if ($remaining === 0) {
        return true;
    }
    $mask = (0xFF << (8 - $remaining)) & 0xFF;
    return (ord($ipBin[$fullBytes]) & $mask) === (ord($netBin[$fullBytes]) & $mask);
}

function request_session(): string
{
    $session = request_param('dbx_session');
    if (!preg_match('/\A[A-Za-z0-9_-]{8,128}\z/', $session)) {
        respond_error(400, 'Invalid tunnel session');
    }
    return $session;
}

function request_param(string $name, string $default = ''): string
{
    if (isset($_GET[$name])) {
        return trim((string) $_GET[$name]);
    }
    if (isset($_POST[$name])) {
        return trim((string) $_POST[$name]);
    }
    return $default;
}

function request_connect_timeout(): int
{
    $timeout = (int) request_param('dbx_connect_timeout', '10');
    return max(1, min(300, $timeout));
}

function base_dir(): string
{
    global $DBX_TUNNEL_DIR;

    return rtrim($DBX_TUNNEL_DIR, '/\\');
}

function session_dir(string $session): string
{
    return base_dir() . DIRECTORY_SEPARATOR . $session;
}

function existing_session_dir(string $session): string
{
    $dir = session_dir($session);
    if (!is_dir($dir) || is_link($dir)) {
        respond_error(404, 'Tunnel session not found');
    }
    return $dir;
}

function ensure_session_alive(string $dir): void
{
    if (is_file($dir . DIRECTORY_SEPARATOR . 'error.txt')) {
        respond_error(502, trim((string) file_get_contents($dir . DIRECTORY_SEPARATOR . 'error.txt')));
    }
    if (is_file($dir . DIRECTORY_SEPARATOR . 'closed')) {
        respond_error(410, 'Tunnel session closed');
    }
}

function touch_heartbeat(string $dir): void
{
    @touch($dir . DIRECTORY_SEPARATOR . 'heartbeat');
}

function session_last_seen(string $dir): int
{
    $latest = 0;
    foreach (['heartbeat', 'meta.json'] as $name) {
        $path = $dir . DIRECTORY_SEPARATOR . $name;
        clearstatcache(true, $path);
        $mtime = @filemtime($path);
        if ($mtime !== false && $mtime > $latest) {
            $latest = $mtime;
        }
    }
    return $latest;
}

function count_active_sessions(): int
{
    global $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS;

    $count = 0;
    foreach (glob(base_dir() . DIRECTORY_SEPARATOR . '*', GLOB_ONLYDIR) ?: [] as $dir) {
        if (is_file($dir . DIRECTORY_SEPARATOR . 'closed')) {
            continue;
        }
        // Allow some slack past the idle timeout for a worker that is about to exit.
        if (time() - session_last_seen($dir) <= $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS + 30) {
            $count++;
        }
    }
    return $count;
}

function default_tunnel_dir(): string
{
    $uid = current_uid();
    return sys_get_temp_dir() . DIRECTORY_SEPARATOR . 'dbx_tunnel' . ($uid === null ? '' : '_' . $uid);
}

function is_windows(): bool
{
    return DIRECTORY_SEPARATOR === '\\';
}

function current_uid(): ?int
{
    if (is_windows()) {
        return null;
    }
    if (function_exists('posix_geteuid')) {
        return posix_geteuid();
    }
    // Without ext-posix, learn the effective uid from the owner of a fresh temp file.
    $probe = @tempnam(sys_get_temp_dir(), 'dbxuid');
    if ($probe === false) {
        return null;
    }
    $owner = @fileowner($probe);
    @unlink($probe);
    return $owner === false ? null : $owner;
}

function ensure_base_dir(): void
{
    $dir = base_dir();
    if (is_link($dir)) {
        internal_error('Tunnel storage is unavailable', 'tunnel base directory is a symlink: ' . $dir);
    }
    if (!is_dir($dir)) {
        $old = umask(0077);
        $created = @mkdir($dir, 0700, true);
        umask($old);
        if (!$created && !is_dir($dir)) {
            internal_error('Tunnel storage is unavailable', 'failed to create tunnel base directory ' . $dir);
        }
    }
    if (is_windows()) {
        return;
    }
    clearstatcache(true, $dir);
    if (is_link($dir)) {
        internal_error('Tunnel storage is unavailable', 'tunnel base directory is a symlink: ' . $dir);
    }
    $uid = current_uid();
    $owner = @fileowner($dir);
    if ($uid === null || $owner === false || $owner !== $uid) {
        internal_error(
            'Tunnel storage is unavailable',
            sprintf('tunnel base directory %s is owned by uid %s, expected %s', $dir, var_export($owner, true), var_export($uid, true))
        );
    }
    $perms = @fileperms($dir);
    if ($perms === false || ($perms & 0077) !== 0) {
        if (!@chmod($dir, 0700)) {
            internal_error('Tunnel storage is unavailable', 'failed to restrict tunnel base directory to 0700: ' . $dir);
        }
    }
}

function cleanup_old_sessions(): void
{
    global $DBX_TUNNEL_MAX_SESSION_SECONDS, $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS;

    $base = base_dir();
    if (!is_dir($base)) {
        return;
    }
    foreach (glob($base . DIRECTORY_SEPARATOR . '*', GLOB_ONLYDIR) ?: [] as $dir) {
        $mtime = @filemtime($dir);
        $lastSeen = max($mtime === false ? 0 : $mtime, session_last_seen($dir));
        $age = time() - $lastSeen;
        $closed = is_file($dir . DIRECTORY_SEPARATOR . 'closed');
        if ($age > ($DBX_TUNNEL_MAX_SESSION_SECONDS * 2) || ($closed && $age > $DBX_TUNNEL_IDLE_TIMEOUT_SECONDS)) {
            remove_dir($dir);
        }
    }
}

function remove_dir(string $dir): void
{
    foreach (glob($dir . DIRECTORY_SEPARATOR . '*') ?: [] as $path) {
        if (is_dir($path) && !is_link($path)) {
            remove_dir($path);
        } else {
            @unlink($path);
        }
    }
    @rmdir($dir);
}

/** @return resource */
function open_lock(string $path)
{
    $lock = @fopen($path, 'c');
    if (!$lock || !flock($lock, LOCK_EX)) {
        internal_error('Tunnel storage is unavailable', 'failed to lock ' . $path);
    }
    return $lock;
}

/** @param resource $lock */
function release_lock($lock): void
{
    if (is_resource($lock)) {
        flock($lock, LOCK_UN);
        fclose($lock);
    }
}

function write_error(string $dir, string $message): void
{
    @file_put_contents($dir . DIRECTORY_SEPARATOR . 'error.txt', $message, LOCK_EX);
}

function mark_closed(string $dir): void
{
    @touch($dir . DIRECTORY_SEPARATOR . 'closed');
}

function log_detail(string $message): void
{
    error_log('dbx_tunnel: ' . $message);
}

function is_digits(string $value): bool
{
    return preg_match('/\A[0-9]+\z/', $value) === 1;
}

function env_bool(string $name, bool $default): bool
{
    $value = getenv($name);
    if ($value === false || trim($value) === '') {
        return $default;
    }
    return in_array(strtolower(trim($value)), ['1', 'true', 'yes', 'on'], true);
}

/** @return string[] */
function env_list(string $name): array
{
    return array_values(array_filter(array_map('trim', explode(',', getenv($name) ?: '')), static function ($v): bool {
        return $v !== '';
    }));
}

function respond_json(array $payload, bool $exit = true): void
{
    header('Content-Type: application/json');
    header('Cache-Control: no-store');
    echo json_encode($payload);
    if ($exit) {
        exit;
    }
}

/** Logs the detail server-side and returns only a generic message to the client. */
function internal_error(string $publicMessage, string $detail): void
{
    log_detail($detail);
    respond_error(500, $publicMessage);
}

function respond_error(int $status, string $message): void
{
    http_response_code($status);
    header('Content-Type: text/plain; charset=utf-8');
    header('Cache-Control: no-store');
    echo $message;
    exit;
}
