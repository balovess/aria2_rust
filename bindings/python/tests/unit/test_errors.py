import pytest

from aria2_rust_client.errors import (
    Aria2Error,
    AuthError,
    ConnectionError,
    RpcError,
    TimeoutError,
)


class TestAria2Error:
    def test_default_code(self):
        err = Aria2Error("something failed")
        assert err.code == -1
        assert str(err) == "something failed"

    def test_custom_code(self):
        err = Aria2Error("custom", code=42)
        assert err.code == 42
        assert str(err) == "custom"

    def test_is_exception(self):
        assert issubclass(Aria2Error, Exception)


class TestConnectionError:
    def test_default_message_and_code(self):
        err = ConnectionError()
        assert err.code == -2
        assert "Connection error" in str(err)

    def test_custom_message(self):
        err = ConnectionError("host unreachable")
        assert err.code == -2
        assert str(err) == "host unreachable"

    def test_inherits_aria2_error(self):
        assert issubclass(ConnectionError, Aria2Error)
        with pytest.raises(Aria2Error):
            raise ConnectionError("fail")


class TestAuthError:
    def test_default_message_and_code(self):
        err = AuthError()
        assert err.code == -3
        assert "Authentication error" in str(err)

    def test_custom_message(self):
        err = AuthError("bad token")
        assert err.code == -3
        assert str(err) == "bad token"

    def test_inherits_aria2_error(self):
        assert issubclass(AuthError, Aria2Error)
        with pytest.raises(Aria2Error):
            raise AuthError("fail")


class TestRpcError:
    def test_default_code(self):
        err = RpcError("method not found")
        assert err.code == -1
        assert str(err) == "method not found"

    def test_custom_code(self):
        err = RpcError("not found", code=1)
        assert err.code == 1

    def test_inherits_aria2_error(self):
        assert issubclass(RpcError, Aria2Error)
        with pytest.raises(Aria2Error):
            raise RpcError("fail")


class TestTimeoutError:
    def test_default_message_and_code(self):
        err = TimeoutError()
        assert err.code == -4
        assert "Request timeout" in str(err)

    def test_custom_message(self):
        err = TimeoutError("30s elapsed")
        assert err.code == -4
        assert str(err) == "30s elapsed"

    def test_inherits_aria2_error(self):
        assert issubclass(TimeoutError, Aria2Error)
        with pytest.raises(Aria2Error):
            raise TimeoutError("fail")


class TestCatchAll:
    def test_all_caught_as_aria2_error(self):
        errors = [
            ConnectionError("c"),
            AuthError("a"),
            RpcError("r"),
            TimeoutError("t"),
        ]
        for err in errors:
            with pytest.raises(Aria2Error):
                raise err
