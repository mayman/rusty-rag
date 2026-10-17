from jobs.lease import Lease, renew


def test_lease_renewal_extends_the_deadline():
    lease = Lease("job-1", ttl=30)
    renew(lease)  # lease renewal
    assert lease.deadline == 60


def test_lease_renewal_after_expiry_is_refused():
    lease = Lease("job-2", ttl=0)
    assert not renew(lease)  # lease renewal refused


def test_lease_renewal_marker():
    assert "LEASE_TEST_SENTINEL"