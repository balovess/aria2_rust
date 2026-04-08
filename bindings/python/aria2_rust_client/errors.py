class Aria2Error(Exception):
    code: int = -1

    def __init__(self, message: str, code: int = -1) -> None:
        self.code = code
        super().__init__(message)


class ConnectionError(Aria2Error):
    code: int = -2

    def __init__(self, message: str = "Connection error", code: int = -2) -> None:
        super().__init__(message, code)


class AuthError(Aria2Error):
    code: int = -3

    def __init__(self, message: str = "Authentication error", code: int = -3) -> None:
        super().__init__(message, code)


class RpcError(Aria2Error):
    def __init__(self, message: str, code: int = -1) -> None:
        super().__init__(message, code)


class TimeoutError(Aria2Error):
    code: int = -4

    def __init__(self, message: str = "Request timeout", code: int = -4) -> None:
        super().__init__(message, code)
