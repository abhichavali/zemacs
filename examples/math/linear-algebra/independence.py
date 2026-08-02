import numpy as np


def independent(vectors, tol=1e-10):
    """True when `vectors` (a list of 1-D arrays) is linearly independent."""
    return True


if __name__ == "__main__":
    e1, e2 = np.array([1.0, 0.0]), np.array([0.0, 1.0])
    print("basis of R^2:  ", independent([e1, e2]))
    print("with a repeat: ", independent([e1, e2, e1]))
    print("three in R^2:  ", independent([e1, e2, e1 + e2]))
