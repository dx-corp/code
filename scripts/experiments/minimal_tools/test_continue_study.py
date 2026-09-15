import unittest
from continue_study import remaining


class ContinuationTests(unittest.TestCase):
    def test_failed_attempts_are_retained_not_retried(self):
        manifest = {'order': [('a',['fast','minimal']),('b',['minimal','fast'])]}
        rows = [{'case':'a','arm':'fast','success':False}, {'case':'a','arm':'minimal','success':True}]
        self.assertEqual(remaining(manifest,rows), [('b','minimal'),('b','fast')])

    def test_reordered_or_duplicate_rows_rejected(self):
        manifest = {'order': [('a',['fast','minimal'])]}
        for rows in ([{'case':'a','arm':'minimal'}], [{'case':'a','arm':'fast'}]*2):
            with self.assertRaisesRegex(ValueError,'prefix'):
                remaining(manifest,rows)
