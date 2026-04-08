import pytest

from aria2_rust_client.types import (
    DownloadEvent,
    DownloadStatus,
    EventType,
    FileInfo,
    GlobalStat,
    SessionInfo,
    StatusInfo,
    UriEntry,
    VersionInfo,
)


class TestEventType:
    def test_all_seven_values(self):
        expected = [
            "DOWNLOAD_START",
            "DOWNLOAD_PAUSE",
            "DOWNLOAD_STOP",
            "DOWNLOAD_COMPLETE",
            "DOWNLOAD_ERROR",
            "BT_DOWNLOAD_COMPLETE",
            "BT_DOWNLOAD_ERROR",
        ]
        values = [e.value for e in EventType]
        assert values == expected

    def test_string_enum(self):
        assert isinstance(EventType.DOWNLOAD_START, str)
        assert EventType.DOWNLOAD_START == "DOWNLOAD_START"


class TestDownloadStatus:
    def test_all_six_values(self):
        expected = ["active", "waiting", "paused", "error", "complete", "removed"]
        values = [s.value for s in DownloadStatus]
        assert values == expected

    def test_string_enum(self):
        assert isinstance(DownloadStatus.ACTIVE, str)
        assert DownloadStatus.ACTIVE == "active"


class TestUriEntry:
    def test_from_dict_full(self):
        data = {"uri": "http://example.com/file.zip", "status": "used"}
        entry = UriEntry.from_dict(data)
        assert entry.uri == "http://example.com/file.zip"
        assert entry.status == "used"

    def test_from_dict_missing_keys(self):
        entry = UriEntry.from_dict({})
        assert entry.uri is None
        assert entry.status is None

    def test_from_dict_extra_keys_ignored(self):
        entry = UriEntry.from_dict({"uri": "http://x", "unknown": 123})
        assert entry.uri == "http://x"
        assert entry.status is None


class TestFileInfo:
    def test_from_dict_with_uris(self):
        data = {
            "index": "1",
            "path": "/tmp/file.zip",
            "length": "1048576",
            "completedLength": "512000",
            "selected": "true",
            "uris": [
                {"uri": "http://example.com/file.zip", "status": "used"},
                {"uri": "http://mirror.com/file.zip", "status": "waiting"},
            ],
        }
        info = FileInfo.from_dict(data)
        assert info.index == "1"
        assert info.path == "/tmp/file.zip"
        assert info.length == "1048576"
        assert info.completed_length == "512000"
        assert info.selected == "true"
        assert len(info.uris) == 2
        assert info.uris[0].uri == "http://example.com/file.zip"
        assert info.uris[1].status == "waiting"

    def test_from_dict_camel_case_keys(self):
        data = {"completedLength": "100", "uris": []}
        info = FileInfo.from_dict(data)
        assert info.completed_length == "100"

    def test_from_dict_missing_keys(self):
        info = FileInfo.from_dict({})
        assert info.index is None
        assert info.path is None
        assert info.uris == []

    def test_from_dict_no_uris(self):
        info = FileInfo.from_dict({"index": "1"})
        assert info.uris == []


class TestStatusInfo:
    def test_from_dict_full(self):
        data = {
            "gid": "2089b05ecca3d829",
            "totalLength": "34896136",
            "completedLength": "34896136",
            "uploadLength": "0",
            "downloadSpeed": "0",
            "uploadSpeed": "0",
            "errorCode": "0",
            "status": "complete",
            "dir": "/downloads",
            "files": [
                {
                    "index": "1",
                    "path": "/downloads/file.zip",
                    "length": "34896136",
                    "completedLength": "34896136",
                    "selected": "true",
                    "uris": [{"uri": "http://example.com/file.zip", "status": "used"}],
                }
            ],
        }
        info = StatusInfo.from_dict(data)
        assert info.gid == "2089b05ecca3d829"
        assert info.total_length == "34896136"
        assert info.completed_length == "34896136"
        assert info.upload_length == "0"
        assert info.download_speed == "0"
        assert info.upload_speed == "0"
        assert info.error_code == "0"
        assert info.status == "complete"
        assert info.dir == "/downloads"
        assert len(info.files) == 1
        assert info.files[0].path == "/downloads/file.zip"
        assert info.files[0].uris[0].uri == "http://example.com/file.zip"

    def test_from_dict_camel_case_conversion(self):
        data = {
            "gid": "abc",
            "totalLength": "100",
            "completedLength": "50",
            "downloadSpeed": "1024",
            "uploadSpeed": "0",
            "errorCode": "0",
        }
        info = StatusInfo.from_dict(data)
        assert info.total_length == "100"
        assert info.completed_length == "50"
        assert info.download_speed == "1024"
        assert info.upload_speed == "0"
        assert info.error_code == "0"

    def test_from_dict_missing_keys(self):
        info = StatusInfo.from_dict({"gid": "abc"})
        assert info.gid == "abc"
        assert info.total_length is None
        assert info.completed_length is None
        assert info.files == []

    def test_from_dict_empty(self):
        info = StatusInfo.from_dict({})
        assert info.gid is None
        assert info.status is None
        assert info.files == []

    def test_from_dict_none_values(self):
        info = StatusInfo.from_dict({"gid": None, "status": None})
        assert info.gid is None
        assert info.status is None

    def test_from_dict_extra_keys_ignored(self):
        info = StatusInfo.from_dict({"gid": "x", "customField": "value"})
        assert info.gid == "x"


class TestGlobalStat:
    def test_from_dict_full(self):
        data = {
            "downloadSpeed": "20480",
            "uploadSpeed": "0",
            "numActive": "2",
            "numWaiting": "3",
            "numStopped": "5",
            "numStoppedTotal": "10",
        }
        stat = GlobalStat.from_dict(data)
        assert stat.download_speed == "20480"
        assert stat.upload_speed == "0"
        assert stat.num_active == "2"
        assert stat.num_waiting == "3"
        assert stat.num_stopped == "5"
        assert stat.num_stopped_total == "10"

    def test_from_dict_missing_keys(self):
        stat = GlobalStat.from_dict({})
        assert stat.download_speed is None
        assert stat.num_active is None


class TestVersionInfo:
    def test_from_dict_full(self):
        data = {
            "version": "1.37.0",
            "enabledFeatures": ["Async DNS", "BitTorrent", "Firefox3 Cookie"],
        }
        info = VersionInfo.from_dict(data)
        assert info.version == "1.37.0"
        assert info.enabled_features == ["Async DNS", "BitTorrent", "Firefox3 Cookie"]

    def test_from_dict_missing_features(self):
        info = VersionInfo.from_dict({"version": "1.0.0"})
        assert info.version == "1.0.0"
        assert info.enabled_features == []

    def test_from_dict_empty(self):
        info = VersionInfo.from_dict({})
        assert info.version is None
        assert info.enabled_features == []

    def test_from_dict_features_not_list(self):
        info = VersionInfo.from_dict({"enabledFeatures": "not-a-list"})
        assert info.enabled_features == []


class TestSessionInfo:
    def test_from_dict(self):
        info = SessionInfo.from_dict({"sessionId": "abc123"})
        assert info.session_id == "abc123"

    def test_from_dict_missing(self):
        info = SessionInfo.from_dict({})
        assert info.session_id is None


class TestDownloadEvent:
    def test_from_rpc_notification_start(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onDownloadStart", {"gid": "2089b05ecca3d829"}
        )
        assert event.event_type == EventType.DOWNLOAD_START
        assert event.gid == "2089b05ecca3d829"

    def test_from_rpc_notification_complete(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onDownloadComplete", {"gid": "abc"}
        )
        assert event.event_type == EventType.DOWNLOAD_COMPLETE
        assert event.gid == "abc"

    def test_from_rpc_notification_error(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onDownloadError", {"gid": "abc", "errorCode": "1"}
        )
        assert event.event_type == EventType.DOWNLOAD_ERROR
        assert event.error_code == "1"

    def test_from_rpc_notification_bt_complete(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onBtDownloadComplete", {"gid": "xyz"}
        )
        assert event.event_type == EventType.BT_DOWNLOAD_COMPLETE

    def test_from_rpc_notification_unknown_method(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onUnknown", {"gid": "x"}
        )
        assert event.event_type == EventType.DOWNLOAD_START

    def test_from_rpc_notification_with_files(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onDownloadComplete",
            {"gid": "abc", "files": [{"index": "1", "path": "/tmp/f"}]},
        )
        assert event.files is not None
        assert len(event.files) == 1
        assert event.files[0].path == "/tmp/f"

    def test_from_rpc_notification_camel_case(self):
        event = DownloadEvent.from_rpc_notification(
            "aria2.onDownloadError", {"gid": "abc", "errorCode": "2"}
        )
        assert event.error_code == "2"
