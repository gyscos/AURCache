/// A remote build worker as reported by the AURCache worker-admin API
/// (`GET /api/workers`).
///
/// Written without code generation (unlike most models in this app) so it can
/// be maintained without running `build_runner`; the JSON shape mirrors the
/// server's `workers` row.
class Worker {
  final int id;
  final String name;

  /// One of `pending`, `approved`, `revoked`.
  final String status;
  final String cert_fingerprint;

  /// Comma-separated native / emulated architectures.
  final String native_arches;
  final String emulated_arches;

  /// Unix seconds of the last heartbeat, if the worker was ever seen.
  final DateTime? last_seen;
  final String? version;

  Worker({
    required this.id,
    required this.name,
    required this.status,
    required this.cert_fingerprint,
    required this.native_arches,
    required this.emulated_arches,
    required this.last_seen,
    required this.version,
  });

  factory Worker.fromJson(Map<String, dynamic> json) => Worker(
    id: json['id'] as int,
    name: json['name'] as String,
    status: json['status'] as String,
    cert_fingerprint: json['cert_fingerprint'] as String? ?? "",
    native_arches: json['native_arches'] as String? ?? "",
    emulated_arches: json['emulated_arches'] as String? ?? "",
    last_seen: json['last_seen'] != null
        ? DateTime.fromMillisecondsSinceEpoch((json['last_seen'] as int) * 1000)
        : null,
    version: json['version'] as String?,
  );

  bool get isPending => status == "pending";
  bool get isApproved => status == "approved";
  bool get isRevoked => status == "revoked";

  factory Worker.dummy() => Worker(
    id: 1,
    name: "builder",
    status: "approved",
    cert_fingerprint: "0000000000000000",
    native_arches: "x86_64",
    emulated_arches: "",
    last_seen: DateTime.now(),
    version: "0.1.0",
  );
}
