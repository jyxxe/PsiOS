CREATE INDEX chat_direct_phase_deadline ON direct_chats(phase,invitation_until);
CREATE INDEX chat_inviter_live ON direct_chats(inviter,phase,invitation_until)
 WHERE phase IN('Active','Invited');
CREATE INDEX chat_invitee_live ON direct_chats(invitee,phase,invitation_until)
 WHERE phase IN('Active','Invited');
CREATE INDEX chat_room_live ON chat_rooms(expires) WHERE phase='Active';
CREATE INDEX chat_room_principal_live ON room_memberships(principal,conversation,phase,invitation_until)
 WHERE phase IN('Active','Invited');
