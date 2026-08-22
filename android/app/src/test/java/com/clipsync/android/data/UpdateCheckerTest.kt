package com.clipsync.android.data

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class UpdateCheckerTest {
    @Test
    fun newerPatchIsDetected() {
        assertTrue(UpdateChecker.isNewer("0.1.6", "0.1.5"))
    }

    @Test
    fun equalAndOlderVersionsAreIgnored() {
        assertFalse(UpdateChecker.isNewer("0.1.5", "0.1.5"))
        assertFalse(UpdateChecker.isNewer("0.1.4", "0.1.5"))
    }

    @Test
    fun malformedVersionsAreIgnored() {
        assertFalse(UpdateChecker.isNewer("latest", "0.1.5"))
        assertFalse(UpdateChecker.isNewer("0.1.6", "dev"))
    }
}
