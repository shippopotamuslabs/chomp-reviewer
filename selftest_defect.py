"""Throwaway defect file for the Forge self-test. Not production code."""


def average_load(total_kg, count):
    # Division by zero: count is never validated.
    return total_kg / count


def find_user_sql(user_id):
    # SQL built by string interpolation: injection-prone.
    return "SELECT * FROM users WHERE id = '%s'" % user_id


API_TOKEN = "sk-live-1234567890abcdef"  # hardcoded credential
