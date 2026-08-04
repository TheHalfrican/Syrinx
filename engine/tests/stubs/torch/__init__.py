"""Minimal torch stand-in — no_grad(), cuda.is_available(), device()."""


class no_grad:  # noqa: N801 — matches torch's own lowercase name
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class device:  # noqa: N801 — matches torch's own lowercase name
    """Enough of torch.device for the worker's _pin_device()."""

    def __init__(self, spec):
        self.spec = str(spec)
        self.type = self.spec.split(":")[0]

    def __str__(self):
        return self.spec

    def __eq__(self, other):
        return isinstance(other, device) and other.spec == self.spec


class _Cuda:
    @staticmethod
    def is_available():
        return False


cuda = _Cuda()
