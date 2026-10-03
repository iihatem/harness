def scale(x, factor):
    if not isinstance(x, (int, float)):
        raise TypeError("x must be a number")
    return x * factor
