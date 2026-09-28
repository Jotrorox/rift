"""All HTTP metric readers retain histogram samples and exact large counters."""

import unittest
from unittest.mock import Mock, patch

import bench
import operations
import pilot
from prometheus import parse_metrics


class MetricsTests(unittest.TestCase):
    def test_http_readers_keep_fractional_histograms_and_exact_integer_counters(self):
        for total in ["2.28", "0.00475821", "4.75821e-3", "0"]:
            body = f"""# TYPE rift_connections_accepted_total counter
rift_connections_accepted_total 9007199254740993
# TYPE rift_login_duration_seconds histogram
rift_login_duration_seconds_bucket{{le="0.005"}} 1
rift_login_duration_seconds_bucket{{le="+Inf"}} 2
rift_login_duration_seconds_sum {total}
rift_login_duration_seconds_count 2
rift_backend_players_online{{backend="lobby with spaces"}} 1

""".encode()
            for module, reader, prefix in [
                    (operations, operations.metrics, ""),
                    (pilot, pilot.scrape, "rift_"),
                    (bench, bench.metrics, "rift_")]:
                with self.subTest(reader=module.__name__, total=total):
                    connection = Mock()
                    response = connection.getresponse.return_value
                    response.status = 200
                    response.read.return_value = body
                    with patch.object(module.http.client, "HTTPConnection", return_value=connection):
                        samples = reader(9090)
                    counter = samples[prefix + "connections_accepted_total"]
                    self.assertIs(type(counter), int)
                    self.assertEqual(counter, 9007199254740993)
                    self.assertEqual(samples[prefix + "login_duration_seconds_sum"], float(total))
                    self.assertIs(type(samples[prefix + "login_duration_seconds_count"]), int)
                    self.assertEqual(samples[prefix + "login_duration_seconds_count"], 2)
                    self.assertEqual(samples[prefix + 'login_duration_seconds_bucket{le="0.005"}'], 1)
                    self.assertEqual(samples[prefix + 'login_duration_seconds_bucket{le="+Inf"}'], 2)
                    self.assertEqual(samples[prefix + 'backend_players_online{backend="lobby with spaces"}'], 1)
                    self.assertEqual(len(samples), 6)
                    connection.request.assert_called_once_with("GET", "/metrics")
                    connection.close.assert_called_once_with()

    def test_invalid_sample_is_not_silently_dropped_and_http_connection_closes(self):
        for module, reader in [(operations, operations.metrics), (pilot, pilot.scrape), (bench, bench.metrics)]:
            with self.subTest(reader=module.__name__):
                connection = Mock()
                response = connection.getresponse.return_value
                response.status = 200
                response.read.return_value = b"rift_login_duration_seconds_sum invalid\n"
                with patch.object(module.http.client, "HTTPConnection", return_value=connection):
                    with self.assertRaises(ValueError):
                        reader(9090)
                connection.close.assert_called_once_with()

    def test_comments_whitespace_and_escaped_labels_are_preserved(self):
        text = '\n  # HELP fixture comment\nrift_backend_players_online{backend="quote\\\" newline\\n"}\t3  \n'
        self.assertEqual(parse_metrics(text), {'rift_backend_players_online{backend="quote\\\" newline\\n"}': 3})


if __name__ == "__main__":
    unittest.main()
