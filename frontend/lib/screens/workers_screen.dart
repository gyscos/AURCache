import 'package:aurcache/providers/workers.dart';
import 'package:aurcache/utils/responsive.dart';
import 'package:dio/dio.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:skeletonizer/skeletonizer.dart';
import 'package:toastification/toastification.dart';

import '../api/API.dart';
import '../api/workers.dart';
import '../components/api/api_builder.dart';
import '../components/table_info.dart';
import '../constants/color_constants.dart';
import '../models/worker.dart';

class WorkersScreen extends StatelessWidget {
  const WorkersScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text("Workers"),
        leading: context.mobile
            ? IconButton(
                icon: const Icon(Icons.menu),
                onPressed: () {
                  Scaffold.of(context).openDrawer();
                },
              )
            : null,
      ),
      body: Padding(
        padding: const EdgeInsets.all(defaultPadding),
        child: Container(
          padding: const EdgeInsets.all(defaultPadding),
          decoration: const BoxDecoration(
            color: secondaryColor,
            borderRadius: BorderRadius.all(Radius.circular(10)),
          ),
          child: SingleChildScrollView(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  "Remote Build Workers",
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                const SizedBox(height: 8),
                Text(
                  "Workers must be approved before they can build. Revoking "
                  "refuses a worker's certificate, releases any packages it "
                  "reserved, and requeues its in-flight builds — it is also how "
                  "a machine is retired, since worker rows are kept so build "
                  "history stays readable.",
                  style: Theme.of(context).textTheme.bodySmall,
                ),
                SizedBox(
                  width: double.infinity,
                  child: APIBuilder(
                    interval: const Duration(seconds: 10),
                    onLoad: () => const Text("no data"),
                    onData: (List<Worker> data) {
                      if (data.isEmpty) {
                        return const TableInfo(
                          title: "No workers have enrolled yet",
                        );
                      }
                      return WorkersTable(data: data);
                    },
                    provider: listWorkersProvider,
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class WorkersTable extends ConsumerStatefulWidget {
  const WorkersTable({super.key, required this.data});
  final List<Worker> data;

  @override
  ConsumerState<WorkersTable> createState() => _WorkersTableState();
}

class _WorkersTableState extends ConsumerState<WorkersTable> {
  /// Worker ids with an approve/revoke request currently in flight. Their
  /// buttons are disabled to prevent double-tap races.
  final Set<int> _pending = {};

  /// Revoked workers are kept forever so build history keeps resolving to the
  /// machine that produced it, which means the list would otherwise grow
  /// without bound. Hide them behind a toggle instead of deleting rows.
  bool _showRetired = false;

  @override
  Widget build(BuildContext context) {
    final retired = widget.data.where((e) => e.isRevoked).length;
    final visible = _showRetired
        ? widget.data
        : widget.data.where((e) => !e.isRevoked).toList(growable: false);

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (retired > 0)
          TextButton.icon(
            onPressed: () => setState(() => _showRetired = !_showRetired),
            icon: Icon(
              _showRetired ? Icons.visibility_off : Icons.visibility,
              size: 18,
            ),
            label: Text(
              _showRetired
                  ? "Hide retired ($retired)"
                  : "Show retired ($retired)",
            ),
          ),
        _table(visible, context),
      ],
    );
  }

  Widget _table(List<Worker> data, BuildContext context) {
    return DataTable(
      horizontalMargin: 12,
      columnSpacing: defaultPadding,
      headingRowColor: WidgetStateProperty.resolveWith<Color?>((states) {
        return const Color(0xff131418);
      }),
      headingRowHeight: 50,
      columns: [
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("ID"))),
        DataColumn(label: Skeleton.keep(child: const Text("Name"))),
        DataColumn(label: Skeleton.keep(child: const Text("Status"))),
        DataColumn(label: Skeleton.keep(child: const Text("Arches"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("Packages"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("Priority"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("Version"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("Last Seen"))),
        DataColumn(label: Skeleton.keep(child: const Text("Action"))),
      ],
      rows: data.map((e) => buildDataRow(e, context)).toList(growable: false),
    );
  }

  DataRow buildDataRow(Worker worker, BuildContext context) {
    final arches = _formatArches(worker);

    return DataRow(
      cells: [
        if (context.desktop) DataCell(Text(worker.id.toString())),
        DataCell(
          Tooltip(message: worker.cert_fingerprint, child: Text(worker.name)),
        ),
        DataCell(_statusChip(worker.status)),
        DataCell(Text(arches)),
        if (context.desktop) DataCell(_affinityChips(worker)),
        if (context.desktop) DataCell(_priorityCell(worker)),
        if (context.desktop) DataCell(Text(worker.version ?? "-")),
        if (context.desktop) DataCell(Text(_formatLastSeen(worker.last_seen))),
        DataCell(_actionButtons(worker)),
      ],
    );
  }

  static String _formatArches(Worker worker) {
    final native = worker.native_arches.trim();
    final emulated = worker.emulated_arches.trim();
    if (native.isEmpty && emulated.isEmpty) return "-";
    if (emulated.isEmpty) return native;
    if (native.isEmpty) return "(+$emulated)";
    return "$native (+$emulated)";
  }

  /// Packages reserved to this worker. Spelled out rather than counted: which
  /// packages are restricted is the whole point of the column.
  Widget _affinityChips(Worker worker) {
    final packages = worker.affinityPackages;
    if (packages.isEmpty) return const Text("-");
    return Wrap(
      spacing: 4,
      runSpacing: 4,
      children: packages
          .map(
            (p) => Container(
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
              decoration: BoxDecoration(
                color: const Color(0xFF2A2D3E),
                borderRadius: BorderRadius.circular(6),
              ),
              child: Text(p, style: const TextStyle(fontSize: 12)),
            ),
          )
          .toList(growable: false),
    );
  }

  /// `0` is the default and means "no preference", so show it as a dash to keep
  /// the column quiet until someone actually tunes the fleet.
  Widget _priorityCell(Worker worker) {
    if (worker.priority == 0) return const Text("-");
    return Tooltip(
      message:
          "Higher priority workers get jobs first; lower ones take over when "
          "these are full or offline.",
      child: Text("${worker.priority}"),
    );
  }

  Widget _statusChip(String status) {
    final color = switch (status) {
      "approved" => const Color(0xFF0A6900),
      "pending" => const Color(0xFF6B43A4),
      _ => const Color(0xFF900A0A),
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        color: color,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Text(status, style: const TextStyle(color: Colors.white)),
    );
  }

  Widget _actionButtons(Worker worker) {
    final busy = _pending.contains(worker.id);
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        if (!worker.isApproved)
          _actionButton(
            label: "Approve",
            color: const Color(0xFF0A6900),
            onPressed: busy
                ? null
                : () => _run(
                    worker.id,
                    () => API.approveWorker(worker.id),
                    "worker approved",
                    "Failed to approve worker!",
                  ),
          ),
        if (!worker.isApproved && !worker.isRevoked) const SizedBox(width: 8),
        if (!worker.isRevoked)
          _actionButton(
            label: "Revoke",
            color: const Color(0xFF900A0A),
            onPressed: busy
                ? null
                : () => _run(
                    worker.id,
                    () => API.revokeWorker(worker.id),
                    "worker revoked",
                    "Failed to revoke worker!",
                  ),
          ),
      ],
    );
  }

  Widget _actionButton({
    required String label,
    required Color color,
    required VoidCallback? onPressed,
  }) {
    return OutlinedButton(
      style: OutlinedButton.styleFrom(
        backgroundColor: color,
        side: BorderSide(color: color, width: 0),
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
        padding: const EdgeInsets.symmetric(
          horizontal: defaultPadding,
          vertical: defaultPadding / 2,
        ),
      ),
      onPressed: onPressed,
      child: Text(label, style: const TextStyle(color: Colors.white)),
    );
  }

  Future<void> _run(
    int workerId,
    Future<bool> Function() action,
    String successMessage,
    String errorMessage,
  ) async {
    if (_pending.contains(workerId)) return;
    setState(() => _pending.add(workerId));
    try {
      // Dio only throws for status >= 300, so a non-200 "success" would
      // otherwise be reported to the operator as a completed approve/revoke.
      final ok = await action();
      toastification.show(
        title: Text(ok ? successMessage : errorMessage),
        autoCloseDuration: Duration(seconds: ok ? 3 : 5),
        type: ok ? ToastificationType.success : ToastificationType.error,
      );
    } on DioException {
      toastification.show(
        title: Text(errorMessage),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
    } finally {
      if (mounted) setState(() => _pending.remove(workerId));
    }
    ref.invalidate(listWorkersProvider);
  }

  static String _formatLastSeen(DateTime? ts) {
    if (ts == null) return "never";
    final diff = DateTime.now().difference(ts);
    if (diff.inSeconds < 60) return "${diff.inSeconds}s ago";
    if (diff.inMinutes < 60) return "${diff.inMinutes}m ago";
    if (diff.inHours < 24) return "${diff.inHours}h ago";
    return "${diff.inDays}d ago";
  }
}
