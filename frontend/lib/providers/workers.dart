import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/API.dart';
import '../api/workers.dart';
import '../models/worker.dart';

/// Classic (non-code-generated) provider for the list of enrolled remote build
/// workers. Written by hand so the Workers page can be maintained without
/// running `build_runner`. `autoDispose` keeps it consistent with the app's
/// other list providers so the data doesn't go stale while the page is closed.
final listWorkersProvider = FutureProvider.autoDispose<List<Worker>>((
  ref,
) async {
  return API.listWorkers();
});
