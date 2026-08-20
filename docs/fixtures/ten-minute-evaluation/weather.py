def status(temp_c):
    if temp_c >= 20:
        return "warm"
    return "cold"


def fahrenheit(temp_c):
    return temp_c * 9 / 5 + 32
