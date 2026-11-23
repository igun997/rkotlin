package dev.rkotlin.hello

import org.junit.Assert.assertEquals
import org.junit.Test

class GreeterTest {
    @Test
    fun greetsByName() {
        assertEquals("Hello, Ann, built remotely with rkotlin", Greeter.greet("Ann"))
    }
}
