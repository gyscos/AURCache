import '../models/worker.dart';
import 'api_client.dart';

extension WorkersAPI on ApiClient {
  Future<List<Worker>> listWorkers() async {
    final resp = await getRawClient().get("/workers");
    final responseObject = resp.data as List;
    return responseObject
        .map((e) => Worker.fromJson(e))
        .toList(growable: false);
  }

  Future<bool> approveWorker(int id) async {
    final resp = await getRawClient().post("/workers/$id/approve");
    return resp.statusCode == 200;
  }

  Future<bool> revokeWorker(int id) async {
    final resp = await getRawClient().post("/workers/$id/revoke");
    return resp.statusCode == 200;
  }
}
