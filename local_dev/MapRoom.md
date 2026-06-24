# this document the process of mapping out a room using AI vision libraries

$ what we are trying to accomplish 
a robot should be able to know exactly where it is in a room and have the ability to navigate it using vision and mapping.

the idea whould be the robot explores the room taking pictures identifying the objects identifying the pixels and then creating a map when the robot moves it identifies the pixels one it sees the same pixels it attemps to close the attach to nodes together and eventually close the loop, 
when it comes to knowing distance ideally we should be able to do this with vision alone by using the needed math to calculate the distance between the robot and the object, this is done by using the size of the object in pixels and the known size of the object in real life, this is called triangulation, these values change as we get closer the image will get bigger and eventually perhaps even take up the whole frame, howerver we should still in theory be able to know that this close up image is part of a small part of a larger image that was not from so close up...

then we can use some baic sencers to detect distance
