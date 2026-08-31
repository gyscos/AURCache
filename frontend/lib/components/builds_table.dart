import 'package:aurcache/utils/responsive.dart';
import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';
import 'package:skeletonizer/skeletonizer.dart';

import '../constants/color_constants.dart';
import '../models/build.dart';
import '../utils/file_formatter.dart';
import '../utils/package_color.dart';
import '../utils/time_formatter.dart';

class BuildsTable extends StatelessWidget {
  const BuildsTable({super.key, required this.data});

  final List<Build> data;

  @override
  Widget build(BuildContext context) {
    return DataTable(
      horizontalMargin: 12,
      columnSpacing: defaultPadding,
      headingRowColor: WidgetStateProperty.resolveWith<Color?>((
        Set<WidgetState> states,
      ) {
        return Color(0xff131418);
      }),
      headingRowHeight: 50,
      columns: [
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Build ID"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Date"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Duration"))),
        DataColumn(label: Skeleton.keep(child: Text("Package Name"))),
        DataColumn(label: Skeleton.keep(child: Text("Version"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Platform"))),
        if (context.desktop)
          DataColumn(label: Skeleton.keep(child: Text("Size")), numeric: true),
        DataColumn(label: Skeleton.keep(child: Text("Status"))),
      ],
      rows: data.map((e) => buildDataRow(context, e)).toList(),
    );
  }

  DataRow buildDataRow(BuildContext context, Build build) {
    return DataRow(
      cells: [
        if (context.desktop) DataCell(Text(build.id.toString())),
        if (context.desktop)
          DataCell(
            Text(
              '${build.start_time.day.toString().padLeft(2, '0')}.${build.start_time.month.toString().padLeft(2, '0')}.${build.start_time.year.toString()}',
            ),
          ),
        if (context.desktop)
          DataCell(Text(build.duration.readableBuildDuration())),
        DataCell(
          Text(build.pkg_name),
          onTap: context.mobile
              ? () => context.push("/build/${build.id}")
              : null,
        ),
        DataCell(Text(build.version)),
        if (context.desktop) DataCell(Text(build.platform)),
        // A dash for every build that produced nothing to measure — failed,
        // running or queued. The status column beside it says which.
        if (context.desktop)
          DataCell(Text(build.size?.readableFileSize() ?? '—')),
        DataCell(
          Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              IconButton(
                icon: Icon(
                  switchSuccessIcon(build.status),
                  color: switchSuccessColor(build.status),
                ),
                tooltip: statusLabel(build.status),
                onPressed: () {
                  context.push("/build/${build.id}");
                },
              ),
              // A stalled build is otherwise indistinguishable from one queued
              // behind a busy worker: same status, same icon, forever.
              if (build.waitingMessage != null)
                Tooltip(
                  message: build.waitingMessage!,
                  child: const Icon(
                    Icons.warning_amber_rounded,
                    color: Color(0xFFE0A800),
                    size: 20,
                  ),
                ),
            ],
          ),
        ),
      ],
    );
  }
}
