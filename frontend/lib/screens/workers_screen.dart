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
                  "Workers must be approved before they can build. Revoke to "
                  "immediately refuse a worker's certificate.",
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

class WorkersTable extends ConsumerWidget {
  const WorkersTable({super.key, required this.data});
  final List<Worker> data;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
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
          DataColumn(label: Skeleton.keep(child: const Text("Version"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: const Text("Last Seen"))),
        DataColumn(label: Skeleton.keep(child: const Text("Action"))),
      ],
      rows: data
          .map((e) => buildDataRow(e, context, ref))
          .toList(growable: false),
    );
  }

  DataRow buildDataRow(Worker worker, BuildContext context, WidgetRef ref) {
    final arches = worker.emulated_arches.isEmpty
        ? worker.native_arches
        : "${worker.native_arches} (+${worker.emulated_arches})";

    return DataRow(
      cells: [
        if (context.desktop) DataCell(Text(worker.id.toString())),
        DataCell(
          Tooltip(
            message: worker.cert_fingerprint,
            child: Text(worker.name),
          ),
        ),
        DataCell(_statusChip(worker.status)),
        DataCell(Text(arches.isEmpty ? "-" : arches)),
        if (context.desktop) DataCell(Text(worker.version ?? "-")),
        if (context.desktop) DataCell(Text(_formatLastSeen(worker.last_seen))),
        DataCell(_actionButtons(worker, ref)),
      ],
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

  Widget _actionButtons(Worker worker, WidgetRef ref) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        if (!worker.isApproved)
          _actionButton(
            label: "Approve",
            color: const Color(0xFF0A6900),
            onPressed: () => _run(
              ref,
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
            onPressed: () => _run(
              ref,
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
    required VoidCallback onPressed,
  }) {
    return OutlinedButton(
      style: OutlinedButton.styleFrom(
        backgroundColor: color,
        side: BorderSide(color: color, width: 0),
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8),
        ),
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
    WidgetRef ref,
    Future<bool> Function() action,
    String successMessage,
    String errorMessage,
  ) async {
    try {
      await action();
      toastification.show(
        title: Text(successMessage),
        autoCloseDuration: const Duration(seconds: 3),
        type: ToastificationType.success,
      );
    } on DioException {
      toastification.show(
        title: Text(errorMessage),
        autoCloseDuration: const Duration(seconds: 5),
        type: ToastificationType.error,
      );
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
